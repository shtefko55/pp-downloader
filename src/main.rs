#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod blur;
mod editor;
mod export;
mod media;
mod project;
mod updater;
mod worker;

use eframe::egui;
use editor::{EditState, Editor};
use project::Project;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
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
    /// Свален файл + IN/OUT в него — за да остане достъпен след рестарт.
    #[serde(default)]
    done: Option<(PathBuf, f64, f64)>,
    /// IN/OUT, маски и последен експорт от редактора.
    #[serde(default)]
    edit: Option<EditState>,
    #[serde(skip)]
    state: Shared,
}

#[derive(Serialize, Deserialize, Clone)]
struct Settings {
    /// Папката, в която се създават проектите (всеки в своя подпапка).
    out_dir: String,
    handles: u32,
    parallel: usize,
    precise: bool,
    max_res: u32,
    crf: u32,
    /// Таймкодът се пише на ръка (иначе — падащи менюта).
    #[serde(default)]
    tc_typed: bool,
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
            tc_typed: false,
        }
    }
}

#[derive(Serialize, Deserialize, Default)]
struct Saved {
    settings: Settings,
    /// Последно отваряните проекти, най-новият първи.
    #[serde(default)]
    recent: Vec<PathBuf>,
    #[serde(default)]
    current: Option<PathBuf>,
    // Списъкът от версиите преди проектите — пренася се в проект „Без име“.
    #[serde(default)]
    jobs: Vec<Job>,
    #[serde(default)]
    next_id: u64,
    #[serde(default)]
    edits: HashMap<String, EditState>,
}

/// Отделен прозорец за бързо сваляне на един откъс, без да се пипа списъкът на проекта.
#[derive(Default)]
struct Quick {
    open: bool,
    url: String,
    name: String,
    start: Tc,
    end: Tc,
    state: Shared,
    cancel: Arc<AtomicBool>,
    /// Редакцията от редактора — пренася се, ако откъсът се добави в проекта.
    edit: Option<EditState>,
}

impl Quick {
    fn status(&self) -> Status {
        self.state.lock().unwrap().status
    }

    fn working(&self) -> bool {
        matches!(self.status(), Status::Queued | Status::Working)
    }

    fn output(&self) -> Option<PathBuf> {
        let st = self.state.lock().unwrap();
        (st.status == Status::Done).then(|| st.output.clone()).flatten()
    }
}

struct App {
    saved: Saved,
    quick: Quick,
    project: Project,
    /// Последно записаното съдържание на проекта — записва се само при промяна.
    last_json: String,
    last_autosave: Instant,
    title: String,
    project_msg: String,
    new_project: Option<String>,
    tools: Tools,
    editors: Vec<Editor>,
    editor_msg: String,
    queue: Arc<Mutex<VecDeque<Task>>>,
    active: Arc<AtomicUsize>,
    cancel: Arc<AtomicBool>,
    update_msg: Arc<Mutex<String>>,
    app_update: Arc<Mutex<updater::UpdateState>>,
    scroll_to_add: bool,
    /// Размерът на прозореца се проверява в първите кадри след пускане.
    size_checks: u32,
    /// Отворен лог на грешка: (заглавие, текст).
    log_view: Option<(String, String)>,
}

const UNNAMED: &str = "Без име";

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
        let mut project_msg = String::new();
        let root = PathBuf::from(&saved.settings.out_dir);
        let mut project = match saved.current.as_deref().map(Project::load) {
            Some(Ok(p)) => p,
            other => {
                if let Some(Err(e)) = other {
                    project_msg = e;
                }
                Project::create(&root, UNNAMED).unwrap_or_else(|e| {
                    project_msg = e;
                    Project { name: UNNAMED.into(), path: std::env::temp_dir().join(format!("{UNNAMED}.{}", project::EXT)), ..Default::default() }
                })
            }
        };
        // Пренасяне на стария общ списък (отпреди проектите).
        if !saved.jobs.is_empty() || !saved.edits.is_empty() {
            let mut edits = std::mem::take(&mut saved.edits);
            for mut job in std::mem::take(&mut saved.jobs) {
                if job.url.trim().is_empty() && job.done.is_none() {
                    continue;
                }
                project.next_id += 1;
                job.id = project.next_id;
                job.edit = job.done.as_ref().and_then(|(p, _, _)| edits.remove(&p.display().to_string()));
                project.jobs.push(job);
            }
            // Редактирани файлове без ред в списъка влизат като „видео от диска“.
            let temp = std::env::temp_dir();
            for (path, edit) in edits {
                let path = PathBuf::from(path);
                if !path.is_file() || path.starts_with(&temp) {
                    continue;
                }
                project.next_id += 1;
                let mut job = new_job(project.next_id);
                job.name = path.file_stem().unwrap_or_default().to_string_lossy().into_owned();
                job.done = Some((path, 0.0, 0.0));
                job.edit = Some(edit);
                project.jobs.push(job);
            }
            let _ = project.save();
        }

        let mut app = App {
            saved,
            quick: Quick::default(),
            project,
            last_json: String::new(),
            last_autosave: Instant::now(),
            title: String::new(),
            project_msg,
            new_project: None,
            tools: Tools::find(),
            editors: Vec::new(),
            editor_msg: String::new(),
            queue: Default::default(),
            active: Default::default(),
            cancel: Default::default(),
            update_msg: Default::default(),
            app_update: Default::default(),
            scroll_to_add: false,
            size_checks: 0,
            log_view: None,
        };
        app.project_opened();
        updater::cleanup();
        app.check_for_update(&cc.egui_ctx);
        // Видео, пуснато върху PP_Downloader.exe, се отваря направо в редактора.
        if let Some(file) = std::env::args().nth(1).map(PathBuf::from).filter(|p| p.is_file()) {
            app.open_file(file, &cc.egui_ctx);
        }
        app
    }

    /// След зареждане на проект: състояния на откъсите, последни проекти, запомняне.
    fn project_opened(&mut self) {
        for job in &mut self.project.jobs {
            let mut st = job.state.lock().unwrap();
            match &job.done {
                Some((path, a, b)) if path.is_file() => {
                    st.status = Status::Done;
                    st.progress = 1.0;
                    st.info = format!("Готово: {}", path.file_name().unwrap_or_default().to_string_lossy());
                    st.output = Some(path.clone());
                    st.marks = (*a, *b);
                }
                Some((path, _, _)) => {
                    st.info = format!(
                        "⚠ Суровият файл липсва ({}) — свали го отново",
                        path.file_name().unwrap_or_default().to_string_lossy()
                    );
                }
                None => {}
            }
        }
        if self.project.jobs.is_empty() {
            self.add_job(String::new(), Tc::default(), Tc::default());
        }
        let path = self.project.path.clone();
        self.saved.recent.retain(|p| p != &path);
        self.saved.recent.insert(0, path.clone());
        self.saved.recent.truncate(12);
        self.saved.current = Some(path);
        self.last_json = self.project.to_json();
    }

    fn check_for_update(&self, ctx: &egui::Context) {
        let (state, ctx) = (self.app_update.clone(), ctx.clone());
        *state.lock().unwrap() = updater::UpdateState::Checking;
        std::thread::spawn(move || {
            let result = updater::check().unwrap_or_else(updater::UpdateState::Failed);
            *state.lock().unwrap() = result;
            ctx.request_repaint();
        });
    }

    /// Бутонът за обновяване на програмата (долу вдясно).
    fn app_update_ui(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        use updater::UpdateState as U;
        let state = self.app_update.lock().unwrap().clone();
        match state {
            U::Idle | U::UpToDate | U::Failed(_) => {
                if ui.add(colored_button("⟳ Провери за обновяване", SLATE).min_size(egui::vec2(0.0, 32.0))).clicked() {
                    self.check_for_update(ctx);
                }
                match state {
                    U::UpToDate => {
                        ui.colored_label(egui::Color32::from_rgb(40, 170, 80), "✔ Последна версия");
                    }
                    U::Failed(e) => {
                        ui.colored_label(egui::Color32::from_rgb(220, 60, 60), "✖ Проверката не успя").on_hover_text(e);
                    }
                    _ => {}
                }
            }
            U::Checking => {
                ui.spinner();
                ui.weak("Проверка за нова версия…");
            }
            U::Available { version, url, notes } => {
                let busy = self.busy() || self.quick.working() || self.editors.iter().any(|e| e.exporting());
                let button = colored_button(&format!("⬇ Обнови до v{version}"), GREEN).min_size(egui::vec2(0.0, 32.0));
                let hover = if busy {
                    "Изчакай свалянето/експорта да приключи.".to_string()
                } else if notes.is_empty() {
                    "Програмата ще се затвори и отвори отново с новата версия. Проектът се записва автоматично.".to_string()
                } else {
                    format!("Какво е новото:\n{notes}")
                };
                if ui.add_enabled(!busy, button).on_hover_text(&hover).on_disabled_hover_text(&hover).clicked() {
                    let (state, ctx) = (self.app_update.clone(), ctx.clone());
                    *state.lock().unwrap() = U::Downloading;
                    std::thread::spawn(move || {
                        let result = match updater::install(&url) {
                            Ok(exe) => U::Ready(exe),
                            Err(e) => U::Failed(e),
                        };
                        *state.lock().unwrap() = result;
                        ctx.request_repaint();
                    });
                }
            }
            U::Downloading => {
                ui.spinner();
                ui.weak("Изтегляне на новата версия…");
            }
            U::Ready(exe) => {
                *self.app_update.lock().unwrap() = U::Idle;
                self.store_edits();
                self.autosave();
                updater::restart(&exe);
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
        ui.weak(format!("v{}", updater::VERSION));
    }

    fn busy(&self) -> bool {
        self.active.load(Ordering::SeqCst) > 0 || !self.queue.lock().unwrap().is_empty()
    }

    fn switch_project(&mut self, result: Result<Project, String>) {
        if self.busy() {
            self.project_msg = "Изчакай свалянето да приключи (или го спри), преди да смениш проекта.".into();
            return;
        }
        match result {
            Ok(p) => {
                self.store_edits();
                if let Err(e) = self.project.save() {
                    self.project_msg = e;
                    return;
                }
                self.editors.clear();
                self.project = p;
                self.project_msg.clear();
                self.project_opened();
            }
            Err(e) => self.project_msg = e,
        }
    }

    fn add_job(&mut self, url: String, start: Tc, end: Tc) {
        self.project.next_id += 1;
        let mut job = new_job(self.project.next_id);
        job.url = url;
        job.start = start;
        job.end = end;
        self.project.jobs.push(job);
    }

    /// Задача за сваляне: таймкодовете се разширяват с handles.
    fn make_task(&self, url: &str, name: &str, start: Tc, end: Tc, state: Shared) -> Task {
        let s = &self.saved.settings;
        let cut = start.secs().saturating_sub(s.handles);
        Task {
            url: url.trim().to_string(),
            name: name.to_string(),
            cut_start: cut,
            cut_end: end.secs() + s.handles,
            label: format!("{}_{}", start.file_label(), end.file_label()),
            marks: (start.secs() as f64 - cut as f64, end.secs() as f64 - cut as f64),
            opts: worker::Options { out_dir: self.project.dir(), precise: s.precise, max_res: s.max_res, crf: s.crf },
            state,
        }
    }

    /// Пуска всички чакащи откъси, или само един (`only`).
    fn start_jobs(&mut self, ctx: &egui::Context, only: Option<u64>) {
        let mut queue = self.queue.lock().unwrap();
        for job in self.project.jobs.iter().filter(|j| only.map_or(true, |id| j.id == id)) {
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
            queue.push_back(self.make_task(&job.url, &job.name, job.start, job.end, job.state.clone()));
        }
        drop(queue);
        worker::ensure_workers(&self.queue, &self.active, &self.cancel, self.saved.settings.parallel, &self.tools, ctx);
    }

    fn start_quick(&mut self, ctx: &egui::Context) {
        if self.quick.url.trim().is_empty() || self.quick.working() {
            return;
        }
        // Нов откъс: предишният остава на диска, но редакцията му вече не се помни тук.
        self.quick.edit = None;
        let q = &self.quick;
        if q.end.secs() <= q.start.secs() {
            let mut st = q.state.lock().unwrap();
            st.status = Status::Failed;
            st.info = "Краят трябва да е след началото".into();
            return;
        }
        let task = self.make_task(&q.url, &q.name, q.start, q.end, q.state.clone());
        q.state.lock().unwrap().status = Status::Queued;
        q.cancel.store(false, Ordering::SeqCst);
        let (tools, cancel, ctx) = (self.tools.clone(), q.cancel.clone(), ctx.clone());
        std::thread::spawn(move || worker::run_task(&task, &tools, &cancel, &ctx));
    }

    /// Сваленият в прозореца откъс влиза в проекта като обикновен ред.
    fn quick_to_project(&mut self) {
        let Some(path) = self.quick.output() else { return };
        self.store_edits();
        if self.project.jobs.iter().any(|j| j.done.as_ref().is_some_and(|d| d.0 == path)) {
            return;
        }
        if self.project.jobs.len() == 1 && self.project.jobs[0].url.trim().is_empty() && self.project.jobs[0].done.is_none() {
            self.project.jobs.clear();
        }
        let q = &mut self.quick;
        self.project.next_id += 1;
        let mut job = new_job(self.project.next_id);
        job.url = q.url.trim().to_string();
        job.name = q.name.clone();
        job.start = q.start;
        job.end = q.end;
        job.edit = q.edit.take();
        {
            let src = q.state.lock().unwrap();
            let mut st = job.state.lock().unwrap();
            st.status = Status::Done;
            st.progress = 1.0;
            st.info = src.info.clone();
            st.output = Some(path.clone());
            st.marks = src.marks;
            job.done = Some((path, src.marks.0, src.marks.1));
        }
        self.project.jobs.push(job);
        *q = Quick { open: q.open, ..Default::default() };
        self.scroll_to_add = true;
    }

    fn quick_ui(&mut self, ctx: &egui::Context) {
        if !self.quick.open {
            return;
        }
        let builder = egui::ViewportBuilder::default()
            .with_title("Единично сваляне")
            .with_inner_size([820.0, 300.0])
            .with_min_inner_size([640.0, 240.0]);
        let mut edit = None;
        let mut start = false;
        let mut to_project = false;
        let mut show_log = None;
        ctx.show_viewport_immediate(egui::ViewportId::from_hash_of("quick"), builder, |ctx, _| {
            if ctx.input(|i| i.viewport().close_requested()) {
                self.quick.open = false;
                self.quick.cancel.store(true, Ordering::SeqCst);
            }
            let handles = self.saved.settings.handles;
            let typed = self.saved.settings.tc_typed;
            let editor_msg = self.editor_msg.clone();
            let q = &mut self.quick;
            let working = q.working();
            let (status, progress, info, marks) = {
                let st = q.state.lock().unwrap();
                (st.status, st.progress, st.info.clone(), st.marks)
            };
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.add_space(4.0);
                ui.label("Сваля един откъс в папката на проекта, без да пипа списъка и без да чака опашката.");
                ui.add_space(4.0);
                let fresh = ui
                    .add_enabled_ui(!working, |ui| {
                        let mut changed = false;
                        ui.horizontal(|ui| {
                            let w = (ui.available_width() - 210.0).max(200.0);
                            let r = ui.add(egui::TextEdit::singleline(&mut q.url).hint_text("Линк (YouTube, bnt.bg …)").desired_width(w));
                            changed |= r.changed() | edit_menu(&r, &mut q.url);
                            let n = ui.add(egui::TextEdit::singleline(&mut q.name).hint_text("Име на файла (по избор)").desired_width(200.0));
                            edit_menu(&n, &mut q.name);
                        });
                        ui.horizontal(|ui| {
                            changed |= tc_ui(ui, ("qstart", 0), "Начало:", &mut q.start, typed);
                            ui.add_space(16.0);
                            changed |= tc_ui(ui, ("qend", 0), "Край:", &mut q.end, typed);
                            ui.add_space(16.0);
                            let (a, b) = (q.start.secs(), q.end.secs());
                            if b > a {
                                let (ca, cb) = (a.saturating_sub(handles), b + handles);
                                ui.weak(format!("Реже се: {} – {}  ({})", fmt_secs(ca), fmt_secs(cb), fmt_secs(cb - ca)));
                            }
                        });
                        changed
                    })
                    .inner;
                if fresh && matches!(status, Status::Done | Status::Failed | Status::Cancelled) {
                    *q.state.lock().unwrap() = Default::default();
                }
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if working {
                        if ui.add(colored_button("⏹ Спри", RED).min_size(egui::vec2(80.0, 30.0))).clicked() {
                            q.cancel.store(true, Ordering::SeqCst);
                        }
                    } else {
                        let ready = !q.url.trim().is_empty() && status != Status::Done;
                        start = ui.add_enabled(ready, colored_button("⬇  Свали", GREEN).min_size(egui::vec2(140.0, 30.0))).clicked();
                    }
                });
                ui.add_space(4.0);
                match status {
                    Status::Queued | Status::Working => {
                        ui.add(egui::ProgressBar::new(progress).text(info).animate(true));
                    }
                    Status::Done => {
                        ui.colored_label(egui::Color32::from_rgb(40, 170, 80), format!("✔ {info}"));
                        if !editor_msg.is_empty() {
                            ui.colored_label(egui::Color32::from_rgb(220, 60, 60), &editor_msg);
                        }
                        ui.horizontal(|ui| {
                            if let Some(p) = q.output() {
                                let label = if q.edit.is_some() { "✂ Редактирай (има промени)" } else { "✂ Редактирай" };
                                if ui.add(colored_button(label, PURPLE).min_size(egui::vec2(0.0, 28.0))).clicked() {
                                    edit = Some((p.clone(), marks, q.edit.clone()));
                                }
                                if ui.button("Покажи файла").clicked() {
                                    show_in_explorer(&p);
                                }
                                to_project = ui
                                    .add(colored_button("➕ Добави в проекта", BLUE).min_size(egui::vec2(0.0, 28.0)))
                                    .on_hover_text("Откъсът (и редакцията му) влиза в списъка на проекта, а прозорецът се изчиства за следващ.")
                                    .clicked();
                                if ui.button("Нов откъс").on_hover_text("Изчисти прозореца (файлът остава на диска)").clicked() {
                                    *q = Quick { open: true, ..Default::default() };
                                }
                            }
                        });
                    }
                    Status::Failed => {
                        ui.colored_label(egui::Color32::from_rgb(220, 60, 60), format!("✖ {info}"));
                        let log = q.state.lock().unwrap().log.clone();
                        ui.horizontal(|ui| {
                            if let Some(log) = log_buttons(ui, log) {
                                show_log = Some(("Единично сваляне".to_string(), log));
                            }
                        });
                    }
                    Status::Idle | Status::Cancelled => {
                        ui.weak(info);
                    }
                }
            });
        });
        if start {
            self.start_quick(ctx);
        }
        if let Some((path, marks, state)) = edit {
            self.open_editor(path, Some(marks), state, ctx);
        }
        if to_project {
            self.quick_to_project();
        }
        if show_log.is_some() {
            self.log_view = show_log;
        }
    }

    /// Прозорец с подробния лог на грешка — за копиране и изпращане.
    fn log_ui(&mut self, ctx: &egui::Context) {
        let Some((title, log)) = &self.log_view else { return };
        let builder = egui::ViewportBuilder::default()
            .with_title(format!("Грешка — {title}"))
            .with_inner_size([820.0, 520.0])
            .with_min_inner_size([480.0, 300.0]);
        let mut close = false;
        ctx.show_viewport_immediate(egui::ViewportId::from_hash_of("error_log"), builder, |ctx, _| {
            close = ctx.input(|i| i.viewport().close_requested());
            egui::TopBottomPanel::bottom("log_actions").show(ctx, |ui| {
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.add(colored_button("📋 Копирай целия лог", BLUE).min_size(egui::vec2(0.0, 30.0))).clicked() {
                        ctx.copy_text(log.clone());
                    }
                    if ui.button("Затвори").clicked() {
                        close = true;
                    }
                    ui.weak("Логът съдържа линка, настройките и пълния текст на грешката.");
                });
                ui.add_space(6.0);
            });
            egui::CentralPanel::default().show(ctx, |ui| {
                egui::ScrollArea::both().auto_shrink(false).show(ui, |ui| {
                    let mut text = log.as_str();
                    ui.add(egui::TextEdit::multiline(&mut text).font(egui::TextStyle::Monospace).desired_width(f32::INFINITY));
                });
            });
        });
        if close {
            self.log_view = None;
        }
    }

    fn stop_all(&mut self) {
        self.cancel.store(true, Ordering::SeqCst);
        for task in self.queue.lock().unwrap().drain(..) {
            let mut st = task.state.lock().unwrap();
            st.status = Status::Idle;
            st.info.clear();
        }
    }

    fn open_editor(&mut self, path: PathBuf, marks: Option<(f64, f64)>, edit: Option<EditState>, ctx: &egui::Context) {
        if let Some(e) = self.editors.iter().find(|e| e.path == path) {
            ctx.send_viewport_cmd_to(e.viewport, egui::ViewportCommand::Focus);
            return;
        }
        match self.new_editor(path, marks, edit, ctx) {
            Ok(e) => {
                self.editor_msg.clear();
                self.editors.push(e);
            }
            Err(e) => self.editor_msg = e,
        }
    }

    fn new_editor(&self, path: PathBuf, marks: Option<(f64, f64)>, edit: Option<EditState>, ctx: &egui::Context) -> Result<Editor, String> {
        let ffmpeg = self.tools.ffmpeg.clone().ok_or("Липсва ffmpeg.exe")?;
        let marks = marks.filter(|(a, b)| b > a);
        Editor::open(path, ffmpeg, self.saved.settings.crf, edit, marks, self.project.exports_dir(), ctx)
            .map_err(|e| format!("Видеото не може да се отвори: {e}"))
    }

    /// Редакторът `idx` зарежда друг клип от проекта в същия прозорец; редакцията на текущия се пази.
    fn switch_editor(&mut self, idx: usize, path: PathBuf, ctx: &egui::Context) {
        if let Some(other) = self.editors.iter().find(|e| e.path == path) {
            ctx.send_viewport_cmd_to(other.viewport, egui::ViewportCommand::Focus);
            return;
        }
        self.store_edits();
        let Some(job) = self.project.jobs.iter().find(|j| j.done.as_ref().is_some_and(|d| d.0 == path)) else { return };
        let (marks, edit) = (job.done.as_ref().map(|d| (d.1, d.2)), job.edit.clone());
        match self.new_editor(path, marks, edit, ctx) {
            Ok(mut e) => {
                e.viewport = self.editors[idx].viewport;
                self.editors[idx] = e;
            }
            Err(e) => self.editors[idx].switch_error = e,
        }
    }

    /// Свалените клипове на проекта, номерирани като в главния списък.
    fn clip_refs(&self) -> Vec<editor::ClipRef> {
        self.project
            .jobs
            .iter()
            .enumerate()
            .filter(|(_, j)| j.state.lock().unwrap().status == Status::Done)
            .filter_map(|(i, j)| {
                let path = j.done.as_ref()?.0.clone();
                let stem = path.file_stem().unwrap_or_default().to_string_lossy().into_owned();
                let name = if j.name.trim().is_empty() { editor::clean_name(&stem) } else { j.name.trim().to_string() };
                let name: String = if name.chars().count() > 34 { name.chars().take(33).chain(['…']).collect() } else { name };
                let detail = if j.url.trim().is_empty() {
                    "видео от диска".to_string()
                } else {
                    format!("{} – {}", fmt_secs(j.start.secs()), fmt_secs(j.end.secs()))
                };
                Some(editor::ClipRef {
                    path,
                    label: format!("{}. {name}", i + 1),
                    detail,
                    edited: j.edit.is_some(),
                    exported: j.edit.as_ref().is_some_and(|e| e.exported.is_some()),
                })
            })
            .collect()
    }

    /// Видео от диска: ако вече е в проекта — отваря неговата редакция, иначе го добавя.
    fn open_file(&mut self, path: PathBuf, ctx: &egui::Context) {
        let existing = self.project.jobs.iter().find(|j| j.done.as_ref().is_some_and(|d| d.0 == path));
        if let Some(job) = existing {
            let (marks, edit) = (job.done.as_ref().map(|d| (d.1, d.2)), job.edit.clone());
            self.open_editor(path, marks, edit, ctx);
            return;
        }
        if self.project.jobs.len() == 1 && self.project.jobs[0].url.trim().is_empty() && self.project.jobs[0].done.is_none() {
            self.project.jobs.clear();
        }
        self.project.next_id += 1;
        let mut job = new_job(self.project.next_id);
        job.name = path.file_stem().unwrap_or_default().to_string_lossy().into_owned();
        job.done = Some((path.clone(), 0.0, 0.0));
        {
            let mut st = job.state.lock().unwrap();
            st.status = Status::Done;
            st.progress = 1.0;
            st.info = format!("Готово: {}", path.file_name().unwrap_or_default().to_string_lossy());
            st.output = Some(path.clone());
        }
        self.project.jobs.push(job);
        self.open_editor(path, None, None, ctx);
    }

    /// Копира IN/OUT, маските и експорта от отворените редактори в откъсите на проекта.
    fn store_edits(&mut self) {
        let quick_out = self.quick.output();
        for e in &self.editors {
            if let Some(job) = self.project.jobs.iter_mut().find(|j| j.done.as_ref().is_some_and(|d| d.0 == e.path)) {
                job.edit = Some(e.state.clone());
            } else if quick_out.as_ref() == Some(&e.path) {
                self.quick.edit = Some(e.state.clone());
            }
        }
    }

    fn autosave(&mut self) {
        self.store_edits();
        let json = self.project.to_json();
        if json != self.last_json {
            match project::write_atomic(&self.project.path, &json) {
                Ok(()) => self.last_json = json,
                Err(e) => self.project_msg = e,
            }
        }
    }

    fn project_ui(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let busy = self.busy();
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("Проект:").strong().size(16.0));
            let mut pick = None;
            egui::ComboBox::from_id_salt("project")
                .width(260.0)
                .selected_text(egui::RichText::new(&self.project.name).strong().size(16.0))
                .show_ui(ui, |ui| {
                    for p in &self.saved.recent {
                        let name = p.file_stem().unwrap_or_default().to_string_lossy();
                        let current = *p == self.project.path;
                        let label = if p.is_file() { name.into_owned() } else { format!("{name} (липсва)") };
                        if ui.selectable_label(current, label).on_hover_text(p.display().to_string()).clicked() && !current {
                            pick = Some(p.clone());
                        }
                    }
                });
            if let Some(p) = pick {
                let result = Project::load(&p);
                if result.is_err() {
                    self.saved.recent.retain(|r| r != &p);
                }
                self.switch_project(result);
            }
            if ui.add_enabled(!busy, colored_button("➕ Нов проект", BLUE)).clicked() {
                self.new_project = Some(String::new());
            }
            if ui.add_enabled(!busy, colored_button("📂 Отвори проект…", SLATE)).clicked() {
                if let Some(p) = rfd::FileDialog::new()
                    .set_directory(&self.saved.settings.out_dir)
                    .add_filter("PP проект", &[project::EXT])
                    .pick_file()
                {
                    let result = Project::load(&p);
                    self.switch_project(result);
                }
            }
            if ui.button("Папка на проекта").clicked() {
                let _ = std::process::Command::new("explorer").arg(self.project.dir()).spawn();
            }
            ui.weak(self.project.path.display().to_string());
        });
        if !self.project_msg.is_empty() {
            ui.colored_label(egui::Color32::from_rgb(220, 60, 60), &self.project_msg);
        }

        if let Some(name) = &mut self.new_project {
            let mut close = false;
            let mut create = false;
            egui::Window::new("Нов проект")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
                .show(ctx, |ui| {
                    ui.label("Име на проекта:");
                    let r = ui.add(egui::TextEdit::singleline(name).hint_text("напр. Предаване 100").desired_width(320.0));
                    r.request_focus();
                    if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        create = true;
                    }
                    let folder = Path::new(&self.saved.settings.out_dir).join(worker::sanitize(name.trim()));
                    ui.weak(format!("Папка: {}", folder.display()));
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        create |= ui.add(colored_button("Създай", GREEN).min_size(egui::vec2(100.0, 28.0))).clicked();
                        close = ui.button("Отказ").clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape));
                    });
                });
            if create {
                let name = name.clone();
                let result = Project::create(Path::new(&self.saved.settings.out_dir), &name);
                self.new_project = None;
                self.switch_project(result);
            } else if close {
                self.new_project = None;
            }
        }
    }

    fn settings_ui(&mut self, ui: &mut egui::Ui) {
        let s = &mut self.saved.settings;
        ui.horizontal(|ui| {
            ui.label("Папка за проекти:");
            ui.add(egui::TextEdit::singleline(&mut s.out_dir).desired_width(420.0))
                .on_hover_text("Всеки нов проект се създава в своя подпапка тук.");
            if ui.button("📁 Избери…").clicked() {
                if let Some(dir) = rfd::FileDialog::new().set_directory(&s.out_dir).pick_folder() {
                    s.out_dir = dir.display().to_string();
                }
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
            ui.separator();
            ui.label("Таймкод:");
            ui.selectable_value(&mut s.tc_typed, false, "▼ Меню").on_hover_text("Часове, минути и секунди от падащи менюта");
            ui.selectable_value(&mut s.tc_typed, true, "⌨ Ръчно")
                .on_hover_text("Пишеш цифрите направо; 9 = 09. Tab минава към следващото поле.");
        });
    }

    fn jobs_ui(&mut self, ui: &mut egui::Ui) {
        let handles = self.saved.settings.handles;
        let typed = self.saved.settings.tc_typed;
        let mut show_log = None;
        let mut remove = None;
        let mut duplicate = None;
        let mut edit = None;
        let mut redownload = None;
        for (idx, job) in self.project.jobs.iter_mut().enumerate() {
            let (status, progress, info, output, marks) = {
                let st = job.state.lock().unwrap();
                (st.status, st.progress, st.info.clone(), st.output.clone(), st.marks)
            };
            // Липсващ суров файл се помни, докато не бъде свален отново.
            match (&output, status) {
                (Some(p), Status::Done) => job.done = Some((p.clone(), marks.0, marks.1)),
                (_, Status::Idle) if job.done.as_ref().is_some_and(|d| !d.0.is_file()) => {}
                _ => job.done = None,
            }
            let busy = matches!(status, Status::Queued | Status::Working);
            let imported = job.url.trim().is_empty() && job.done.is_some();
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.add_enabled_ui(!busy, |ui| {
                    ui.horizontal(|ui| {
                        ui.strong(format!("{:>2}.", idx + 1));
                        let w = (ui.available_width() - 330.0).max(200.0);
                        let hint = if imported { "(видео от диска)" } else { "Линк (YouTube, bnt.bg …)" };
                        let r = ui.add(egui::TextEdit::singleline(&mut job.url).hint_text(hint).desired_width(w));
                        let pasted = edit_menu(&r, &mut job.url);
                        let n = ui.add(egui::TextEdit::singleline(&mut job.name).hint_text("Име на файла (по избор)").desired_width(200.0));
                        edit_menu(&n, &mut job.name);
                        if ui.button("⧉").on_hover_text("Дублирай (същият линк, друг откъс)").clicked() {
                            duplicate = Some(idx);
                        }
                        if ui.button("✖").on_hover_text("Премахни от проекта").clicked() {
                            remove = Some(idx);
                        }
                        if (r.changed() || pasted) && matches!(status, Status::Done | Status::Failed | Status::Cancelled) {
                            *job.state.lock().unwrap() = Default::default();
                        }
                    });
                    if !imported {
                        ui.horizontal(|ui| {
                            let changed_a = tc_ui(ui, ("start", job.id), "Начало:", &mut job.start, typed);
                            ui.add_space(16.0);
                            let changed_b = tc_ui(ui, ("end", job.id), "Край:", &mut job.end, typed);
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
                    }
                });
                let exported = job.edit.as_ref().and_then(|e| e.exported.clone());
                match status {
                    Status::Idle => {
                        if !info.is_empty() {
                            ui.horizontal(|ui| {
                                ui.colored_label(egui::Color32::from_rgb(230, 150, 30), &info);
                                if !job.url.trim().is_empty() && ui.add(colored_button("⬇ Свали отново", GREEN)).clicked() {
                                    redownload = Some(job.id);
                                }
                            });
                        }
                        if job.edit.is_some() {
                            ui.weak("✂ Редакцията (IN/OUT, маски) е запазена и ще се приложи след сваляне.");
                        }
                    }
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
                                let label = if job.edit.is_some() { "✂ Редактирай (има промени)" } else { "✂ Редактирай" };
                                if ui.add(colored_button(label, PURPLE)).clicked() {
                                    edit = Some((p.clone(), marks, job.edit.clone()));
                                }
                                if ui.small_button("Покажи файла").clicked() {
                                    show_in_explorer(&p);
                                }
                            }
                            if let Some(x) = &exported {
                                ui.separator();
                                let name = x.file_name().unwrap_or_default().to_string_lossy();
                                if x.is_file() {
                                    ui.label(format!("➜ Експортиран: {name}"));
                                    if ui.small_button("Покажи").clicked() {
                                        show_in_explorer(x);
                                    }
                                } else {
                                    ui.weak(format!("➜ {name} (файлът липсва — експортирай отново)"));
                                }
                            }
                        });
                    }
                    Status::Failed => {
                        ui.horizontal(|ui| {
                            ui.colored_label(egui::Color32::from_rgb(220, 60, 60), format!("✖ {info}"));
                            let log = job.state.lock().unwrap().log.clone();
                            if let Some(log) = log_buttons(ui, log) {
                                show_log = Some((format!("Откъс {}", idx + 1), log));
                            }
                        });
                    }
                    Status::Cancelled => {
                        ui.weak(info);
                    }
                }
            });
        }
        if let Some(i) = duplicate {
            let (url, name, end) = {
                let j = &self.project.jobs[i];
                (j.url.clone(), j.name.clone(), j.end)
            };
            self.project.next_id += 1;
            let mut job = new_job(self.project.next_id);
            job.url = url;
            job.name = name;
            job.start = end;
            job.end = end;
            self.project.jobs.insert(i + 1, job);
        }
        if let Some(i) = remove {
            let path = self.project.jobs[i].done.as_ref().map(|d| d.0.clone());
            self.editors.retain(|e| Some(&e.path) != path.as_ref());
            self.project.jobs.remove(i);
        }
        if let Some((path, marks, state)) = edit {
            self.open_editor(path, Some(marks), state, ui.ctx());
        }
        if let Some(id) = redownload {
            self.start_jobs(ui.ctx(), Some(id));
        }
        if show_log.is_some() {
            self.log_view = show_log;
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
const PURPLE: egui::Color32 = egui::Color32::from_rgb(120, 80, 170);
const AMBER: egui::Color32 = egui::Color32::from_rgb(170, 115, 30);
const SLATE: egui::Color32 = egui::Color32::from_rgb(75, 85, 100);

fn colored_button(text: &str, fill: egui::Color32) -> egui::Button<'static> {
    egui::Button::new(egui::RichText::new(text).color(egui::Color32::WHITE).strong()).fill(fill)
}

fn show_in_explorer(path: &Path) {
    let _ = std::process::Command::new("explorer").raw_arg(format!("/select,\"{}\"", path.display())).spawn();
}

fn new_job(id: u64) -> Job {
    Job {
        id,
        url: String::new(),
        name: String::new(),
        start: Tc::default(),
        end: Tc::default(),
        done: None,
        edit: None,
        state: Default::default(),
    }
}

/// Десен бутон върху текстово поле: Постави (заменя съдържанието), Копирай, Изчисти. Връща дали текстът е сменен.
fn edit_menu(r: &egui::Response, text: &mut String) -> bool {
    let mut changed = false;
    r.context_menu(|ui| {
        if ui.button("📋 Постави").clicked() {
            if let Some(t) = arboard::Clipboard::new().ok().and_then(|mut c| c.get_text().ok()) {
                *text = t.trim().to_string();
                changed = true;
            }
            ui.close_menu();
        }
        if ui.add_enabled(!text.is_empty(), egui::Button::new("Копирай")).clicked() {
            ui.ctx().copy_text(text.clone());
            ui.close_menu();
        }
        if ui.add_enabled(!text.is_empty(), egui::Button::new("✖ Изчисти")).clicked() {
            text.clear();
            changed = true;
            ui.close_menu();
        }
    });
    changed
}

/// Бутоните до грешка; връща лога, ако е натиснат „Детайли“.
fn log_buttons(ui: &mut egui::Ui, log: String) -> Option<String> {
    if log.is_empty() {
        return None;
    }
    if ui.small_button("📋 Копирай лога").on_hover_text("Пълният лог (линк, настройки, грешка) отива в клипборда").clicked() {
        ui.ctx().copy_text(log.clone());
    }
    ui.small_button("🔍 Детайли…").clicked().then_some(log)
}

/// ЧЧ:ММ:СС — падащи менюта или (`typed`) полета за писане + поле за бързо въвеждане на таймкод.
fn tc_ui(ui: &mut egui::Ui, id: (&str, u64), label: &str, tc: &mut Tc, typed: bool) -> bool {
    let before = *tc;
    ui.label(label);
    let combo = |ui: &mut egui::Ui, salt: &str, v: &mut u32, max: u32, hint: &str| {
        if typed {
            typed_field(ui, egui::Id::new((id, salt, "typed")), v, max, hint);
            return;
        }
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

/// Поле за едно число от таймкода: при фокус всичко се маркира, „9“ = 09, над максимума → максимума.
fn typed_field(ui: &mut egui::Ui, fid: egui::Id, v: &mut u32, max: u32, hint: &str) {
    let buf_id = fid.with("buf");
    let focused = ui.memory(|m| m.has_focus(fid));
    let mut text: String = if focused { ui.data_mut(|d| d.get_temp(buf_id)).unwrap_or_default() } else { format!("{:02}", *v) };
    let out = egui::TextEdit::singleline(&mut text)
        .id(fid)
        .desired_width(26.0)
        .char_limit(2)
        .horizontal_align(egui::Align::Center)
        .show(ui);
    if out.response.gained_focus() {
        let mut state = out.state;
        let n = text.chars().count();
        state.cursor.set_char_range(Some(egui::text::CCursorRange::two(egui::text::CCursor::new(0), egui::text::CCursor::new(n))));
        state.store(ui.ctx(), fid);
    }
    text.retain(|c| c.is_ascii_digit());
    if out.response.changed() {
        if let Ok(n) = text.parse::<u32>() {
            *v = n.min(max);
        }
    }
    if out.response.lost_focus() && text.is_empty() {
        *v = 0;
    }
    ui.data_mut(|d| d.insert_temp(buf_id, text));
    out.response.on_hover_text(hint);
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let active = self.active.load(Ordering::SeqCst);
        if active == 0 {
            self.cancel.store(false, Ordering::SeqCst);
        } else {
            worker::ensure_workers(&self.queue, &self.active, &self.cancel, self.saved.settings.parallel, &self.tools, ctx);
        }

        // Запомненият размер може да е миниатюрен (напр. затворен минимизиран) — тогава се връща нормалният.
        if self.size_checks < 10 {
            self.size_checks += 1;
            let (inner, minimized) = ctx.input(|i| (i.viewport().inner_rect, i.viewport().minimized.unwrap_or(false)));
            if !minimized && inner.is_some_and(|r| r.width() < 800.0 || r.height() < 400.0) {
                ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(1100.0, 760.0)));
                ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(egui::pos2(80.0, 60.0)));
                self.size_checks = 10;
            }
        }

        let title = format!("PP Downloader — {}", self.project.name);
        if title != self.title {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
            self.title = title;
        }

        let busy = self.busy();
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
                    if ui
                        .add(colored_button("⚡ Единично сваляне", GREEN).min_size(big))
                        .on_hover_text("Отделен прозорец: свали и редактирай един откъс, без да пипаш списъка")
                        .clicked()
                    {
                        self.quick.open = true;
                        ctx.send_viewport_cmd_to(egui::ViewportId::from_hash_of("quick"), egui::ViewportCommand::Focus);
                    }
                    if ui.add(colored_button("📂 Отвори видео…", PURPLE).min_size(big)).on_hover_text("Добави видео от диска в проекта и го редактирай").clicked() {
                        if let Some(p) = rfd::FileDialog::new()
                            .set_directory(self.project.dir())
                            .add_filter("Видео", &["mp4", "mov", "mkv", "m4v", "webm"])
                            .pick_file()
                        {
                            self.open_file(p, ctx);
                        }
                    }
                    if ui.add(colored_button("Изчисти готовите", AMBER).min_size(big)).on_hover_text("Махни свалените откъси от списъка на проекта").clicked() {
                        self.project.jobs.retain(|j| j.state.lock().unwrap().status != Status::Done);
                        if self.project.jobs.is_empty() {
                            self.add_job(String::new(), Tc::default(), Tc::default());
                        }
                    }
                    ui.weak(self.update_msg.lock().unwrap().as_str());
                });
            });
            ui.separator();
            self.project_ui(ui, ctx);
            ui.separator();
            if !self.editor_msg.is_empty() {
                ui.colored_label(egui::Color32::from_rgb(220, 60, 60), &self.editor_msg);
            }
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
                    self.start_jobs(ctx, None);
                }
                if ui.add_enabled(busy, colored_button("⏹ Спри", RED).min_size(egui::vec2(80.0, 32.0))).clicked() {
                    self.stop_all();
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    self.app_update_ui(ui, ctx);
                    ui.separator();
                    let jobs = &self.project.jobs;
                    let count = |s: Status| jobs.iter().filter(|j| j.state.lock().unwrap().status == s).count();
                    let exported = jobs.iter().filter(|j| j.edit.as_ref().is_some_and(|e| e.exported.is_some())).count();
                    ui.weak(format!(
                        "{} откъса · {} свалени · {} експортирани · {} грешки",
                        jobs.len(),
                        count(Status::Done),
                        exported,
                        count(Status::Failed)
                    ));
                });
            });
            ui.add_space(6.0);
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| self.jobs_ui(ui));
        });

        self.quick_ui(ctx);
        self.log_ui(ctx);
        let clips = self.clip_refs();
        for e in &mut self.editors {
            e.show(ctx, &clips);
        }
        let switches: Vec<(usize, PathBuf)> =
            self.editors.iter_mut().enumerate().filter_map(|(i, e)| Some((i, e.switch_to.take()?))).collect();
        for (i, path) in switches {
            self.switch_editor(i, path, ctx);
        }
        if self.editors.iter().any(|e| !e.open) {
            self.store_edits();
            self.editors.retain(|e| e.open);
        }

        if self.last_autosave.elapsed() >= Duration::from_secs(1) {
            self.last_autosave = Instant::now();
            self.autosave();
        }
        // Автоматичното записване се проверява поне веднъж на 2 секунди.
        ctx.request_repaint_after(Duration::from_secs(2));
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        self.autosave();
        eframe::set_value(storage, eframe::APP_KEY, &self.saved);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.stop_all();
        self.autosave();
    }
}

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1100.0, 760.0]).with_min_inner_size([800.0, 400.0]),
        ..Default::default()
    };
    eframe::run_native("PP Downloader", options, Box::new(|cc| Ok(Box::new(App::new(cc)))))
}
