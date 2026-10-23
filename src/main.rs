//! Memrium Converter: turn pictures into another format, with the settings the quick "Convert to" of the file manager leaves
//! out (quality, size, what a see-through picture is put over, where the result goes). The work is
//! `memrium_de::fsops::convert_image_with`, on a thread of its own.
//!
//!   memrium_convert <pictures...>

use chebrium::app::{self, App, Config, Host};
use chebrium::draw::Align;
use chebrium::ui::Ui;
use memrium_de::fsops::{self, human_size, ConvertOptions, CONVERT_TO};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc::{channel, Receiver},
    Arc,
};
use std::time::{Duration, Instant};

const PAD: f32 = 24.0;
const HEAD_H: f32 = 84.0;
const BAR_H: f32 = 92.0;

enum Msg {
    /// pictures done so far, and the one being worked on
    Progress(usize, String),
    /// pictures made, and the ones that failed (name: why)
    Done(usize, Vec<String>),
}

struct Job {
    rx: Receiver<Msg>,
    cancel: Arc<AtomicBool>,
    done: usize,
    total: usize,
    current: String,
}

struct ConvertApp {
    sources: Vec<PathBuf>,
    /// index in `CONVERT_TO`
    format: usize,
    quality: f32,
    png_level: usize,
    size: usize,
    background: usize,
    dest: String,
    job: Option<Job>,
    note: String,
    /// the folder to show once it is done
    shown: Option<PathBuf>,
    waker: Option<app::Waker>,
    want_quit: bool,
}

/// What the size row offers: (label, longest side).
const SIZES: [(&str, Option<u32>); 5] = [("Original", None), ("3840", Some(3840)), ("1920", Some(1920)), ("1280", Some(1280)), ("800", Some(800))];

fn page_color(dark: bool) -> [f32; 3] {
    if dark { [0.115, 0.115, 0.115] } else { [0.955, 0.960, 0.972] }
}

fn card_color(dark: bool) -> [f32; 3] {
    if dark { [0.165, 0.165, 0.165] } else { [1.0, 1.0, 1.0] }
}

fn card(ui: &mut Ui<'_>, r: [f32; 4], radius: f32) {
    let ink = ui.ink;
    ui.fill(r, radius, ink, 0.09);
    ui.fill([r[0] + 1.0, r[1] + 1.0, r[2] - 2.0, r[3] - 2.0], radius - 1.0, card_color(ui.dark), 1.0);
}

fn chip(ui: &mut Ui<'_>, x: f32, y: f32, text: &str) -> f32 {
    let w = ui.measure_raw(text, 11.5, true) + 16.0;
    let ink = ui.ink;
    ui.fill([x, y, w, 20.0], 10.0, ink, 0.08);
    ui.text_raw(x + 8.0, y + 14.5, text, 11.5, true, ink, 0.62, Align::Left);
    w + 6.0
}

fn heading(ui: &mut Ui<'_>, x: f32, y: f32, text: &str) {
    let ink = ui.ink;
    ui.text(x, y, text, 11.5, true, ink, 0.50, Align::Left);
}

fn fit(ui: &mut Ui<'_>, text: &str, size: f32, bold: bool, max: f32) -> String {
    if ui.measure_raw(text, size, bold) <= max {
        return text.to_string();
    }
    let mut s = text.to_string();
    while !s.is_empty() && ui.measure_raw(&format!("{s}…"), size, bold) > max {
        s.pop();
    }
    format!("{s}…")
}

fn size_of(p: &Path) -> u64 {
    std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)
}

impl ConvertApp {
    fn new(args: Vec<String>) -> ConvertApp {
        let sources: Vec<PathBuf> = args.into_iter().map(PathBuf::from).filter(|p| fsops::can_convert(p)).collect();
        let dest = sources.first().and_then(|p| p.parent()).map(|p| p.to_string_lossy().to_string()).unwrap_or_else(|| fsops::home().to_string_lossy().to_string());
        // JPEG is the usual answer to "something smaller"; PNG when the picture is a PNG already
        let own = sources.first().and_then(|p| p.extension()).map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
        let format = if own == "png" { 1 } else { 0 };
        ConvertApp { sources, format, quality: 90.0, png_level: 1, size: 0, background: 0, dest, job: None, note: String::new(), shown: None, waker: None, want_quit: false }
    }

    fn options(&self) -> ConvertOptions {
        ConvertOptions {
            quality: self.quality.round().clamp(1.0, 100.0) as u8,
            png_level: self.png_level as u8,
            max_side: SIZES[self.size].1,
            background: if self.background == 1 { [0, 0, 0] } else { [255, 255, 255] },
            out_dir: Some(PathBuf::from(self.dest.trim())),
        }
    }

    fn start(&mut self) {
        if self.job.is_some() || self.sources.is_empty() {
            return;
        }
        let dir = PathBuf::from(self.dest.trim());
        if self.dest.trim().is_empty() || (dir.exists() && !dir.is_dir()) {
            self.note = "Choose a folder for the pictures".into();
            return;
        }
        if let Err(e) = std::fs::create_dir_all(&dir) {
            self.note = format!("Cannot use that folder: {e}");
            return;
        }
        let (opts, ext, sources) = (self.options(), CONVERT_TO[self.format].1, self.sources.clone());
        let (tx, rx) = channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let (c2, waker) = (cancel.clone(), self.waker.clone());
        let total = sources.len();
        std::thread::Builder::new()
            .name("convert-job".into())
            .spawn(move || {
                let (mut made, mut failed) = (0usize, Vec::new());
                let mut last = Instant::now() - Duration::from_secs(1);
                for (i, p) in sources.iter().enumerate() {
                    if c2.load(Ordering::Relaxed) {
                        break;
                    }
                    let name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                    if last.elapsed() >= Duration::from_millis(40) {
                        last = Instant::now();
                        let _ = tx.send(Msg::Progress(i, name.clone()));
                        if let Some(w) = &waker {
                            w.wake();
                        }
                    }
                    let stem = p.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "picture".into());
                    match fsops::convert_image_with(p, &format!("{stem}.{ext}"), &opts) {
                        Ok(_) => made += 1,
                        Err(e) => failed.push(format!("{name}: {e}")),
                    }
                }
                let _ = tx.send(Msg::Done(made, failed));
                if let Some(w) = &waker {
                    w.wake();
                }
            })
            .ok();
        self.note.clear();
        self.shown = None;
        self.job = Some(Job { rx, cancel, done: 0, total, current: String::new() });
        self.shown = Some(dir);
    }

    fn poll(&mut self) -> bool {
        let Some(job) = &mut self.job else { return false };
        let mut changed = false;
        let mut finished = None;
        while let Ok(m) = job.rx.try_recv() {
            changed = true;
            match m {
                Msg::Progress(i, name) => {
                    job.done = i;
                    job.current = name;
                }
                Msg::Done(made, failed) => finished = Some((made, failed)),
            }
        }
        if let Some((made, failed)) = finished {
            self.job = None;
            self.note = match failed.first() {
                None => format!("Converted {made} picture{}", if made == 1 { "" } else { "s" }),
                Some(first) => format!("Converted {made}; could not convert {}: {first}", failed.len()),
            };
            if made == 0 {
                self.shown = None;
            }
        }
        changed
    }
}

impl App for ConvertApp {
    fn start(&mut self, waker: app::Waker) {
        self.waker = Some(waker);
    }

    fn background(&self, look: &chebrium::theme::Appearance) -> ([f32; 3], f32) {
        (page_color(look.dark), 1.0)
    }

    fn tick(&mut self, host: &mut Host) {
        if self.poll() {
            host.redraw = true;
        }
        if self.want_quit {
            host.quit = true;
        }
    }

    fn key(&mut self, key: chebrium::input::Key, _mods: chebrium::input::KeyMods, host: &mut Host) {
        host.redraw = true;
        if key == chebrium::input::Key::Esc {
            if let Some(j) = &self.job {
                j.cancel.store(true, Ordering::Relaxed);
            }
        }
    }

    fn draw(&mut self, ui: &mut Ui<'_>, size: [f32; 2], host: &mut Host) {
        let [w, h] = size;
        let (ink, accent) = (ui.ink, ui.accent);
        host.title = Some("Convert pictures".into());
        if self.sources.is_empty() {
            let tile = [w / 2.0 - 36.0, h / 2.0 - 110.0, 72.0, 72.0];
            ui.fill(tile, 22.0, accent, 0.16);
            ui.glyph(w / 2.0, h / 2.0 - 74.0, 34.0, "image", accent, 1.0);
            ui.text(w / 2.0, h / 2.0 - 4.0, "No pictures to convert", 20.0, true, ink, 0.92, Align::Center);
            ui.text(w / 2.0, h / 2.0 + 24.0, "Choose Convert to ▸ More… on some pictures in Files.", 13.0, false, ink, 0.55, Align::Center);
            return;
        }

        // ---- the header: the pictures
        let total: u64 = self.sources.iter().map(|s| size_of(s)).sum();
        ui.fill([PAD, 18.0, 48.0, 48.0], 14.0, accent, 0.18);
        ui.glyph(PAD + 24.0, 42.0, 24.0, "image", accent, 1.0);
        ui.text(PAD + 62.0, 36.0, "Convert pictures", 18.0, true, ink, 0.96, Align::Left);
        let n = self.sources.len();
        let mut x = PAD + 62.0;
        x += chip(ui, x, 46.0, &format!("{n} picture{}", if n == 1 { "" } else { "s" }));
        chip(ui, x, 46.0, &human_size(total));
        ui.fill([0.0, HEAD_H - 1.0, w, 1.0], 0.0, ink, 0.09);

        // ---- the format: a card each, with a word about it
        let mut y = HEAD_H + 22.0;
        heading(ui, PAD, y, "Format");
        y += 12.0;
        let notes = ["Sharp, see-through", "Small photos", "Modern, small", "Uncompressed", "256 colours", "Print and archive", "Icons, up to 256 px"];
        let (cols, gap) = (4usize, 10.0);
        let cw = (w - 2.0 * PAD - gap * (cols as f32 - 1.0)) / cols as f32;
        let ch = 56.0;
        for (i, (label, _)) in CONVERT_TO.iter().enumerate() {
            let (c, r) = (i % cols, i / cols);
            let rect = [PAD + c as f32 * (cw + gap), y + r as f32 * (ch + gap), cw, ch];
            let on = self.format == i;
            let hov = ui.hit(rect);
            let t = ui.ease(50_000 + i as u64, if on { 1.0 } else { 0.0 }, 18.0);
            let wash = ui.ease(51_000 + i as u64, if hov { 1.0 } else { 0.0 }, 22.0);
            card(ui, rect, 12.0);
            if wash > 0.01 && !on {
                ui.fill(rect, 12.0, ink, 0.05 * wash);
            }
            if t > 0.01 {
                ui.fill(rect, 12.0, accent, 0.55 * t);
                ui.fill([rect[0] + 2.0, rect[1] + 2.0, rect[2] - 4.0, rect[3] - 4.0], 10.0, card_color(ui.dark), 1.0);
                ui.fill([rect[0] + 2.0, rect[1] + 2.0, rect[2] - 4.0, rect[3] - 4.0], 10.0, accent, 0.12 * t);
            }
            ui.text_raw(rect[0] + 14.0, rect[1] + 24.0, label, 14.0, true, if on { accent } else { ink }, 0.95, Align::Left);
            ui.text(rect[0] + 14.0, rect[1] + 43.0, notes[i], 11.5, false, ink, 0.52, Align::Left);
            if ui.click && hov {
                self.format = i;
            }
        }
        y += 2.0 * ch + gap + 26.0;

        // ---- the settings that belong to the format, on one card
        heading(ui, PAD, y, "Settings");
        y += 12.0;
        let ext = CONVERT_TO[self.format].1;
        let alpha_free = matches!(ext, "jpg" | "bmp");
        let mut rows: Vec<&str> = vec!["size"];
        if ext == "jpg" {
            rows.push("quality");
        }
        if ext == "png" {
            rows.push("png");
        }
        if alpha_free {
            rows.push("background");
        }
        let row_h = 48.0;
        let area = [PAD, y, w - 2.0 * PAD, rows.len() as f32 * row_h + 8.0];
        card(ui, area, 12.0);
        for (i, kind) in rows.iter().enumerate() {
            let ry = area[1] + 4.0 + i as f32 * row_h;
            let right = area[0] + area[2] - 16.0;
            if i > 0 {
                ui.fill([area[0] + 16.0, ry, area[2] - 32.0, 1.0], 0.0, ink, 0.07);
            }
            let label_y = ry + row_h / 2.0 + 4.5;
            match *kind {
                "size" => {
                    ui.text(area[0] + 16.0, label_y, "Longest side (px)", 13.0, false, ink, 0.9, Align::Left);
                    let labels: Vec<&str> = SIZES.iter().map(|s| s.0).collect();
                    let mut sel = self.size;
                    if ui.segmented([right - 330.0, ry + 8.0, 330.0, 32.0], &labels, &mut sel) {
                        self.size = sel;
                    }
                }
                "quality" => {
                    ui.text(area[0] + 16.0, label_y, "Quality", 13.0, false, ink, 0.9, Align::Left);
                    ui.slider([right - 330.0, ry + 8.0, 330.0, 32.0], &mut self.quality, 1.0, 100.0, "%");
                }
                "png" => {
                    ui.text(area[0] + 16.0, label_y, "Compression", 13.0, false, ink, 0.9, Align::Left);
                    let mut sel = self.png_level;
                    if ui.segmented([right - 330.0, ry + 8.0, 330.0, 32.0], &["Fast", "Balanced", "Smallest"], &mut sel) {
                        self.png_level = sel;
                    }
                }
                _ => {
                    ui.text(area[0] + 16.0, label_y, "See-through parts become", 13.0, false, ink, 0.9, Align::Left);
                    let mut sel = self.background;
                    if ui.segmented([right - 330.0, ry + 8.0, 330.0, 32.0], &["White", "Black"], &mut sel) {
                        self.background = sel;
                    }
                }
            }
        }
        y += area[3] + 18.0;
        if ext == "webp" {
            ui.text(PAD, y + 4.0, "WebP is written without loss here, so it is bigger than a JPEG of the same picture.", 11.5, false, ink, 0.5, Align::Left);
        } else if ext == "gif" {
            ui.text(PAD, y + 4.0, "A GIF has 256 colours: photos look rough in it. Only the first frame of an animation is kept.", 11.5, false, ink, 0.5, Align::Left);
        }

        // ---- the bottom: where they go, how it is going, and the button
        let bar = [0.0, h - BAR_H, w, BAR_H];
        ui.fill(bar, 0.0, card_color(ui.dark), 1.0);
        ui.fill([0.0, bar[1], w, 1.0], 0.0, ink, 0.09);
        if let Some(job) = &self.job {
            let frac = (job.done as f32 / job.total.max(1) as f32).clamp(0.0, 1.0);
            let track = [PAD, bar[1] + 24.0, w - 2.0 * PAD - 120.0, 8.0];
            ui.fill(track, 4.0, ink, 0.10);
            ui.fill([track[0], track[1], (track[2] * frac).max(8.0), track[3]], 4.0, accent, 1.0);
            let what = fit(ui, &format!("Converting {}…", job.current), 12.5, false, w - 2.0 * PAD - 130.0);
            ui.text_raw(PAD, bar[1] + 58.0, &what, 12.5, false, ink, 0.75, Align::Left);
            ui.text_raw(PAD, bar[1] + 76.0, &format!("{} of {}", job.done.min(job.total), job.total), 11.5, false, ink, 0.5, Align::Left);
            if ui.button([w - PAD - 100.0, bar[1] + 16.0, 100.0, 34.0], "Cancel", false) {
                job.cancel.store(true, Ordering::Relaxed);
            }
            return;
        }
        ui.glyph(PAD + 8.0, bar[1] + 26.0, 16.0, "folder", ink, 0.55);
        ui.text(PAD + 26.0, bar[1] + 30.0, "Save in", 12.0, true, ink, 0.55, Align::Left);
        let mut dest = self.dest.clone();
        let field = [PAD + 96.0, bar[1] + 12.0, w - 2.0 * PAD - 96.0, 30.0];
        ui.push_clip([field[0] - 3.0, field[1] - 3.0, field[2] + 6.0, field[3] + 6.0]);
        ui.text_field(field, "dest", &mut dest);
        ui.pop_clip();
        self.dest = dest;
        let done = !self.note.is_empty() && self.shown.is_some();
        let bad = !done && !self.note.is_empty();
        let msg = fit(ui, &self.note, 12.5, false, w - 2.0 * PAD - 320.0);
        ui.text_raw(PAD, bar[1] + 70.0, &msg, 12.5, false, if bad { [0.95, 0.45, 0.35] } else { ink }, if self.note.is_empty() { 0.0 } else { 0.85 }, Align::Left);
        let mut bx = w - PAD;
        let b = [bx - 160.0, bar[1] + 50.0, 160.0, 34.0];
        if ui.button(b, if done { "Done" } else { "Convert" }, true) {
            if done {
                self.want_quit = true;
            } else {
                self.start();
            }
        }
        bx -= 172.0;
        if done {
            let b = [bx - 130.0, bar[1] + 50.0, 130.0, 34.0];
            if ui.button(b, "Show in Files", false) {
                if let Some(p) = self.shown.clone() {
                    let mut c = std::process::Command::new(fsops::own_program("memrium_files"));
                    c.arg(p);
                    let _ = c.spawn();
                }
            }
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    app::run(Config::new("Memrium Converter", "memrium-convert").size(700, 700).min_size(600, 560), ConvertApp::new(args));
}
