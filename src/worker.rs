//! Сваляне и рязане на един откъс: yt-dlp намира потоците, ffmpeg сваля само нужния участък.

use serde_json::Value;
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const CANCELLED: &str = "Спряно";

#[derive(Clone, Copy, Default, PartialEq, Debug)]
pub enum Status {
    #[default]
    Idle,
    Queued,
    Working,
    Done,
    Failed,
    Cancelled,
}

#[derive(Default)]
pub struct JobState {
    pub status: Status,
    pub progress: f32,
    pub info: String,
    pub output: Option<PathBuf>,
    /// Въведените начало и край спрямо началото на сваления клип (без handles).
    pub marks: (f64, f64),
}

pub type Shared = Arc<Mutex<JobState>>;

#[derive(Clone)]
pub struct Options {
    pub out_dir: PathBuf,
    pub precise: bool,
    pub max_res: u32,
    pub crf: u32,
}

pub struct Task {
    pub url: String,
    pub name: String,
    /// Реални граници на рязане (с handles), в секунди.
    pub cut_start: u32,
    pub cut_end: u32,
    /// Таймкодовете, въведени от потребителя — за името на файла.
    pub label: String,
    pub marks: (f64, f64),
    pub opts: Options,
    pub state: Shared,
}

#[derive(Clone)]
pub struct Tools {
    pub ytdlp: Option<PathBuf>,
    pub ffmpeg: Option<PathBuf>,
}

impl Tools {
    pub fn find() -> Self {
        Tools { ytdlp: find_tool("yt-dlp"), ffmpeg: find_tool("ffmpeg") }
    }
}

fn find_tool(name: &str) -> Option<PathBuf> {
    let file = format!("{name}.exe");
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(exe_dir) = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf)) {
        dirs.push(exe_dir.join("bin"));
        dirs.push(exe_dir.clone());
    }
    if let Ok(cwd) = std::env::current_dir() {
        dirs.push(cwd.join("bin"));
    }
    if let Some(path) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&path));
    }
    dirs.into_iter().map(|d| d.join(&file)).find(|p| p.is_file())
}

/// Команда без конзолен прозорец; папката на инструмента се добавя в PATH (за deno, ffmpeg и т.н.).
pub(crate) fn command(tool: &Path) -> Command {
    let mut cmd = Command::new(tool);
    cmd.creation_flags(CREATE_NO_WINDOW).stdin(Stdio::null());
    if let Some(dir) = tool.parent() {
        let mut paths = vec![dir.to_path_buf()];
        if let Some(p) = std::env::var_os("PATH") {
            paths.extend(std::env::split_paths(&p));
        }
        if let Ok(joined) = std::env::join_paths(paths) {
            cmd.env("PATH", joined);
        }
    }
    cmd.env("PYTHONIOENCODING", "utf-8");
    cmd
}

fn tail(text: &str, lines: usize) -> String {
    let v: Vec<&str> = text.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    v[v.len().saturating_sub(lines)..].join("\n")
}

/// Пуска процес, връща stdout; при грешка — последните редове от stderr.
pub(crate) fn run_capture(cmd: Command, cancel: &AtomicBool) -> Result<String, String> {
    run_bytes(cmd, cancel).map(|b| String::from_utf8_lossy(&b).into_owned())
}

pub(crate) fn run_bytes(mut cmd: Command, cancel: &AtomicBool) -> Result<Vec<u8>, String> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| format!("Не може да се стартира: {e}"))?;
    let mut out = child.stdout.take().unwrap();
    let mut err = child.stderr.take().unwrap();
    let t_out = thread::spawn(move || {
        let mut b = Vec::new();
        let _ = out.read_to_end(&mut b);
        b
    });
    let t_err = thread::spawn(move || {
        let mut b = Vec::new();
        let _ = err.read_to_end(&mut b);
        b
    });
    let status = loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(CANCELLED.into());
        }
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) => thread::sleep(Duration::from_millis(15)),
            Err(e) => return Err(e.to_string()),
        }
    };
    let out = t_out.join().unwrap_or_default();
    let err = String::from_utf8_lossy(&t_err.join().unwrap_or_default()).into_owned();
    if status.success() {
        Ok(out)
    } else {
        Err(tail(&err, 4))
    }
}

pub fn update_ytdlp(tools: &Tools) -> String {
    let Some(yt) = &tools.ytdlp else { return "yt-dlp не е намерен".into() };
    match run_capture(
        {
            let mut c = command(yt);
            c.arg("-U");
            c
        },
        &AtomicBool::new(false),
    ) {
        Ok(out) => tail(&out, 1),
        Err(e) => format!("Грешка при обновяване: {e}"),
    }
}

/// Стартира работници, докато има задачи в опашката и свободни места.
pub fn ensure_workers(
    queue: &Arc<Mutex<VecDeque<Task>>>,
    active: &Arc<AtomicUsize>,
    cancel: &Arc<AtomicBool>,
    parallel: usize,
    tools: &Tools,
    ctx: &eframe::egui::Context,
) {
    while active.load(Ordering::SeqCst) < parallel && !queue.lock().unwrap().is_empty() {
        active.fetch_add(1, Ordering::SeqCst);
        let (queue, active, cancel, tools, ctx) =
            (queue.clone(), active.clone(), cancel.clone(), tools.clone(), ctx.clone());
        thread::spawn(move || {
            loop {
                let Some(task) = queue.lock().unwrap().pop_front() else { break };
                run_task(&task, &tools, &cancel, &ctx);
            }
            active.fetch_sub(1, Ordering::SeqCst);
            ctx.request_repaint();
        });
    }
}

/// Сваля един откъс и записва резултата в състоянието му.
pub fn run_task(task: &Task, tools: &Tools, cancel: &AtomicBool, ctx: &eframe::egui::Context) {
    {
        let mut st = task.state.lock().unwrap();
        st.status = Status::Working;
        st.progress = 0.0;
        st.info = "Анализ на линка…".into();
    }
    ctx.request_repaint();
    let result = process(task, tools, cancel, ctx);
    let mut st = task.state.lock().unwrap();
    match result {
        Ok(path) => {
            st.status = Status::Done;
            st.progress = 1.0;
            st.info = format!("Готово: {}", path.file_name().unwrap_or_default().to_string_lossy());
            st.output = Some(path);
            st.marks = task.marks;
        }
        Err(e) if e == CANCELLED => {
            st.status = Status::Cancelled;
            st.info = CANCELLED.into();
        }
        Err(e) => {
            st.status = Status::Failed;
            st.info = e;
        }
    }
    drop(st);
    ctx.request_repaint();
}

struct Stream {
    url: String,
    headers: String,
    vcodec: String,
    acodec: String,
}

impl Stream {
    fn from_json(v: &Value) -> Option<Self> {
        let url = v["url"].as_str()?.to_string();
        let headers = v["http_headers"]
            .as_object()
            .map(|h| h.iter().filter_map(|(k, v)| Some(format!("{k}: {}\r\n", v.as_str()?))).collect())
            .unwrap_or_default();
        let codec = |key: &str| v[key].as_str().unwrap_or("").to_lowercase();
        Some(Stream { url, headers, vcodec: codec("vcodec"), acodec: codec("acodec") })
    }
    fn has_video(&self) -> bool {
        self.vcodec != "none"
    }
    fn has_audio(&self) -> bool {
        self.acodec != "none"
    }
}

fn is_h264(codec: &str) -> bool {
    codec.starts_with("avc") || codec.starts_with("h264")
}

/// YouTube понякога връща 403 за току-що получен линк към потока — тогава се взима нов.
fn process(task: &Task, tools: &Tools, cancel: &AtomicBool, ctx: &eframe::egui::Context) -> Result<PathBuf, String> {
    const ATTEMPTS: u32 = 4;
    for attempt in 1.. {
        match process_once(task, tools, cancel, ctx) {
            Err(e) if attempt < ATTEMPTS && (e.contains("403") || e.contains("Forbidden")) => {
                task.state.lock().unwrap().info = format!("Сървърът отказа (403), нов опит {}/{ATTEMPTS}…", attempt + 1);
                ctx.request_repaint();
                thread::sleep(Duration::from_secs(2));
            }
            r => return r,
        }
    }
    unreachable!()
}

fn process_once(task: &Task, tools: &Tools, cancel: &AtomicBool, ctx: &eframe::egui::Context) -> Result<PathBuf, String> {
    let yt = tools.ytdlp.as_ref().ok_or("Липсва yt-dlp.exe (сложи го в папка bin до програмата)")?;
    let ff = tools.ffmpeg.as_ref().ok_or("Липсва ffmpeg.exe (сложи го в папка bin до програмата)")?;
    let set_info = |info: String, progress: Option<f32>| {
        let mut st = task.state.lock().unwrap();
        st.info = info;
        if let Some(p) = progress {
            st.progress = p;
        }
        drop(st);
        ctx.request_repaint();
    };

    // 1. Избор на формат: най-висока резолюция и fps, при равни — H.264 и AAC.
    let res = if task.opts.max_res > 0 { format!("res:{}", task.opts.max_res) } else { "res".into() };
    let sort = format!("{res},fps,vcodec:h264,acodec:aac");
    // Сайтове без собствен модул в yt-dlp (напр. bnt.bg): видеото се търси в HTML-а на страницата.
    // Защитната стена на bnt.bg (FortiWeb) пропуска само заявки, които изглеждат като от браузър:
    // пълен User-Agent, Referer и бисквитката от страницата — същото, което праща разширението в браузъра.
    let (info, page_title, cookie) = match resolve(yt, &task.url, &sort, None, cancel) {
        Ok(info) => (info, None, String::new()),
        Err(e) if e.contains("Unsupported URL") => {
            let (html, cookie) = fetch_page(&task.url, cancel)?;
            let html = html.replace("\\/", "/");
            let media = find_media_url(&html, &task.url).ok_or_else(|| {
                if is_block_page(&html) {
                    "Сайтът блокира заявката (защитна стена). Изчакай няколко минути и опитай пак.".to_string()
                } else {
                    format!("Не е намерено видео на страницата ({e})")
                }
            })?;
            (resolve(yt, &media, &sort, Some(&task.url), cancel)?, find_og_title(&html), cookie)
        }
        Err(e) => return Err(e),
    };

    let mut streams: Vec<Stream> = match info["requested_formats"].as_array() {
        Some(arr) => arr.iter().filter_map(Stream::from_json).collect(),
        None => Stream::from_json(&info).into_iter().collect(),
    };
    if !cookie.is_empty() {
        for s in &mut streams {
            s.headers += &format!("Cookie: {cookie}\r\n");
        }
    }
    // Директен файл: yt-dlp не знае кодеците, питаме ffprobe.
    let mut height = info["height"].as_u64();
    if let [s] = streams.as_mut_slice() {
        if s.vcodec.is_empty() {
            if let Some(probe) = probe(ff, s, cancel) {
                for st in probe["streams"].as_array().into_iter().flatten() {
                    let name = st["codec_name"].as_str().unwrap_or("").to_string();
                    match st["codec_type"].as_str() {
                        Some("video") if s.vcodec.is_empty() => {
                            s.vcodec = name;
                            height = height.or(st["height"].as_u64());
                        }
                        Some("audio") if s.acodec.is_empty() => s.acodec = if name == "aac" { "mp4a".into() } else { name },
                        _ => {}
                    }
                }
            }
        }
    }
    let vi = streams.iter().position(Stream::has_video).ok_or("Не е намерен видео поток")?;
    let ai = streams.iter().position(|s| s.has_audio() && !s.has_video()).or_else(|| streams[vi].has_audio().then_some(vi));

    let mut cut_end = task.cut_end as f64;
    if let Some(d) = info["duration"].as_f64() {
        if task.cut_start as f64 >= d {
            return Err(format!("Началото е след края на видеото ({})", fmt_secs(d as u32)));
        }
        cut_end = cut_end.min(d);
    }
    let start = task.cut_start as f64;
    let dur = cut_end - start;

    let vcodec = &streams[vi].vcodec;
    let acodec = ai.map(|i| streams[i].acodec.clone()).unwrap_or_default();
    let copy_video = !task.opts.precise && is_h264(vcodec);
    let copy_audio = copy_video && acodec.starts_with("mp4a");
    let height = height.map(|h| format!("{h}p ")).unwrap_or_default();
    let codec_short = vcodec.split('.').next().unwrap_or("?").to_string();
    let mode = if copy_video { "копиране" } else { "прекодиране в H.264" };
    let fmt_desc = format!("{height}{codec_short} → {mode}");

    // 2. Име на файла.
    let title = page_title.as_deref().or(info["title"].as_str()).unwrap_or("video");
    let base = sanitize(if task.name.trim().is_empty() { title } else { task.name.trim() });
    std::fs::create_dir_all(&task.opts.out_dir).map_err(|e| format!("Папката не може да се създаде: {e}"))?;
    let out = unique_path(&task.opts.out_dir, &format!("{base} [{}]", task.label));
    let tmp = out.with_extension("part.mp4");

    // 3. ffmpeg сваля само участъка и го записва като MP4.
    let mut cmd = command(ff);
    cmd.args(["-hide_banner", "-nostdin", "-y", "-loglevel", "error", "-progress", "pipe:1", "-nostats"]);
    let used: Vec<usize> = match ai {
        Some(a) if a != vi => vec![vi, a],
        _ => vec![vi],
    };
    for &i in &used {
        let s = &streams[i];
        if s.url.starts_with("http") {
            if !s.headers.is_empty() {
                cmd.arg("-headers").arg(&s.headers);
            }
            // Заявки по 10 MB: YouTube бави една дълга заявка до ~0.6 MB/s.
            cmd.args(["-request_size", "10000000"]);
        }
        cmd.arg("-ss").arg(format!("{start:.3}")).arg("-t").arg(format!("{dur:.3}")).arg("-i").arg(&s.url);
    }
    cmd.args(["-map", "0:v:0"]);
    match ai {
        Some(a) if a != vi => cmd.args(["-map", "1:a:0"]),
        Some(_) => cmd.args(["-map", "0:a:0"]),
        None => &mut cmd,
    };
    if copy_video {
        cmd.args(["-c:v", "copy", "-avoid_negative_ts", "make_zero"]);
    } else {
        cmd.args(["-c:v", "libx264", "-preset", "medium", "-crf"])
            .arg(task.opts.crf.to_string())
            .args(["-pix_fmt", "yuv420p", "-profile:v", "high", "-g"])
            // Ключов кадър всяка секунда — по-бързо превъртане в редактора.
            .arg(info["fps"].as_f64().map_or(25, |f| f.round().max(1.0) as u32).to_string());
    }
    if copy_audio {
        cmd.args(["-c:a", "copy"]);
    } else {
        cmd.args(["-c:a", "aac", "-b:a", "320k"]);
    }
    cmd.args(["-movflags", "+faststart", "-f", "mp4"]).arg(&tmp);

    set_info(format!("{fmt_desc} — сваляне…"), Some(0.0));
    let result = run_ffmpeg(cmd, dur, cancel, |p| set_info(format!("{fmt_desc} — {:.0}%", p * 100.0), Some(p)));
    if let Err(e) = result {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    std::fs::rename(&tmp, &out).map_err(|e| format!("Преименуване: {e}"))?;
    Ok(out)
}

const BROWSER_UA: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36";

/// `referer` — страницата, от която е взет линкът към видеото (заявката изглежда като от браузъра).
fn resolve(yt: &Path, url: &str, sort: &str, referer: Option<&str>, cancel: &AtomicBool) -> Result<Value, String> {
    let mut cmd = command(yt);
    cmd.args(["-J", "--no-playlist", "--no-warnings", "--encoding", "utf-8", "-f", "bv*+ba/b", "-S", sort]);
    if let Some(r) = referer {
        cmd.args(["--referer", r, "--user-agent", BROWSER_UA]);
    }
    cmd.arg(url);
    let json = run_capture(cmd, cancel)?;
    serde_json::from_str(&json).map_err(|e| format!("Неразбираем отговор от yt-dlp: {e}"))
}

/// HTML-ът на страницата и бисквитките, които сайтът е дал („име=стойност; …“).
fn fetch_page(url: &str, cancel: &AtomicBool) -> Result<(String, String), String> {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    let jar = std::env::temp_dir().join(format!("pp_cookies_{}_{nanos}.txt", std::process::id()));
    let mut cmd = Command::new("curl.exe");
    cmd.creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .args(["-sSL", "--compressed", "--max-time", "30", "-A", BROWSER_UA])
        .args(["-H", "Accept: text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8"])
        .args(["-H", "Accept-Language: bg,en;q=0.8"])
        .arg("-c")
        .arg(&jar)
        .arg(url);
    let html = run_capture(cmd, cancel).map_err(|e| format!("Страницата не може да се зареди: {e}"));
    let cookie = std::fs::read_to_string(&jar).map(|j| cookie_header(&j)).unwrap_or_default();
    let _ = std::fs::remove_file(&jar);
    Ok((html?, cookie))
}

/// Netscape cookie файл на curl → стойност за заглавката Cookie.
fn cookie_header(jar: &str) -> String {
    jar.lines()
        .map(|l| l.strip_prefix("#HttpOnly_").unwrap_or(l))
        .filter(|l| !l.starts_with('#'))
        .filter_map(|l| {
            let f: Vec<&str> = l.split('\t').collect();
            (f.len() >= 7).then(|| format!("{}={}", f[5], f[6].trim_end()))
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// Страница на защитната стена вместо истинската.
fn is_block_page(html: &str) -> bool {
    let h = html.to_lowercase();
    ["web page blocked", "request rejected", "access denied", "attack id", "fortiweb"].iter().any(|s| h.contains(s))
}

/// Първият линк към .m3u8 или .mp4 в страницата, като абсолютен URL.
fn find_media_url(html: &str, page_url: &str) -> Option<String> {
    let is_delim = |c: char| matches!(c, '"' | '\'' | '(' | ')' | '<' | '>' | '=' | ' ' | '\n' | '\r' | '\t' | ',');
    let origin = page_url.splitn(4, '/').take(3).collect::<Vec<_>>().join("/");
    let mut found = Vec::new();
    for ext in [".m3u8", ".mp4"] {
        for (pos, _) in html.match_indices(ext) {
            let begin = html[..pos].rfind(is_delim).map_or(0, |i| i + 1);
            let end = html[pos..].find(is_delim).map_or(html.len(), |i| pos + i);
            let raw = html[begin..end].replace("&amp;", "&");
            let url = if raw.starts_with("http") {
                raw
            } else if raw.starts_with("//") {
                format!("https:{raw}")
            } else if raw.starts_with('/') {
                format!("{origin}{raw}")
            } else {
                continue;
            };
            found.push((pos, url));
        }
        if !found.is_empty() {
            break;
        }
    }
    found.into_iter().min_by_key(|(pos, _)| *pos).map(|(_, url)| url)
}

fn probe(ff: &Path, s: &Stream, cancel: &AtomicBool) -> Option<Value> {
    let mut cmd = command(&ff.with_file_name("ffprobe.exe"));
    cmd.args(["-v", "error", "-show_entries", "stream=codec_type,codec_name,height", "-of", "json"]);
    if s.url.starts_with("http") && !s.headers.is_empty() {
        cmd.arg("-headers").arg(&s.headers);
    }
    cmd.arg(&s.url);
    serde_json::from_str(&run_capture(cmd, cancel).ok()?).ok()
}

/// og:title (атрибутите може да са в произволен ред), иначе <title>.
fn find_og_title(html: &str) -> Option<String> {
    let og = html.find("og:title").and_then(|pos| {
        let tag_start = html[..pos].rfind('<')?;
        let tag = &html[tag_start..pos + html[pos..].find('>')?];
        let content = tag.find("content=\"")? + 9;
        Some(&tag[content..content + tag[content..].find('"')?])
    });
    let title = og.or_else(|| {
        let start = html.find("<title>")? + 7;
        Some(&html[start..start + html[start..].find("</title>")?])
    })?;
    let title = title
        .replace("&quot;", "\"")
        .replace("&#039;", "'")
        .replace("&#39;", "'")
        .replace("&amp;", "&");
    let title = title.trim().trim_end_matches(" - Българска национална телевизия").trim().to_string();
    (!title.is_empty()).then_some(title)
}

pub(crate) fn run_ffmpeg(mut cmd: Command, dur: f64, cancel: &AtomicBool, on_progress: impl Fn(f32)) -> Result<(), String> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| format!("ffmpeg не може да се стартира: {e}"))?;
    let stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let (tx, rx) = mpsc::channel::<f32>();
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Some(us) = line.strip_prefix("out_time_us=").and_then(|v| v.trim().parse::<f64>().ok()) {
                let _ = tx.send(((us / 1e6) / dur).clamp(0.0, 1.0) as f32);
            }
        }
    });
    let t_err = thread::spawn(move || {
        let mut b = Vec::new();
        let _ = stderr.read_to_end(&mut b);
        String::from_utf8_lossy(&b).into_owned()
    });
    let status = loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(CANCELLED.into());
        }
        if let Ok(p) = rx.recv_timeout(Duration::from_millis(150)) {
            on_progress(p);
        }
        if let Ok(Some(s)) = child.try_wait() {
            break s;
        }
    };
    let err = t_err.join().unwrap_or_default();
    if status.success() {
        Ok(())
    } else {
        Err(format!("ffmpeg: {}", tail(&err, 4)))
    }
}

pub fn fmt_secs(t: u32) -> String {
    format!("{:02}:{:02}:{:02}", t / 3600, t / 60 % 60, t % 60)
}

pub(crate) fn sanitize(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| if c.is_control() || r#"<>:"/\|?*"#.contains(c) { '_' } else { c })
        .take(120)
        .collect();
    let s = s.trim().trim_end_matches(['.', ' ']).to_string();
    if s.is_empty() { "video".into() } else { s }
}

fn unique_path(dir: &Path, base: &str) -> PathBuf {
    let mut p = dir.join(format!("{base}.mp4"));
    let mut n = 2;
    while p.exists() {
        p = dir.join(format!("{base} ({n}).mp4"));
        n += 1;
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cookies_from_curl_jar() {
        let jar = "# Netscape HTTP Cookie File\n# https://curl.se/docs/http-cookies.html\n\n#HttpOnly_bnt.bg\tFALSE\t/\tFALSE\t1824700000\tcookiesession1\t678B76\nbnt.bg\tFALSE\t/\tFALSE\t0\tlang\tbg\r\n";
        assert_eq!(cookie_header(jar), "cookiesession1=678B76; lang=bg");
        assert_eq!(cookie_header(""), "");
    }

    /// Реално сваляне от YouTube: `cargo test -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn download_clip() {
        let tools = Tools::find();
        let out_dir = std::env::temp_dir().join("pp_downloader_test");
        let cases = [
            ("https://www.youtube.com/watch?v=tItHYpuY0Cg", true),
            ("https://www.youtube.com/watch?v=tItHYpuY0Cg", false),
            ("https://bnt.bg/news/po-sveta-i-u-nas-900-chasa-21-oktomvri-2026-g-352171news.html", true),
            ("https://bnt.bg/news/po-sveta-i-u-nas-900-chasa-21-oktomvri-2026-g-352171news.html", false),
        ];
        for (url, precise) in cases {
            let task = Task {
                url: url.into(),
                name: String::new(),
                cut_start: 100,
                cut_end: 130,
                label: format!("test_{precise}_{}", url.len()),
                marks: (5.0, 25.0),
                opts: Options { out_dir: out_dir.clone(), precise, max_res: 0, crf: 16 },
                state: Default::default(),
            };
            let r = process(&task, &tools, &AtomicBool::new(false), &eframe::egui::Context::default());
            println!("precise={precise}: {r:?} / {}", task.state.lock().unwrap().info);
            assert!(r.unwrap().is_file());
        }
    }
}
