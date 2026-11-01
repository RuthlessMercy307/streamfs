//! Live stats overlay for sfs-mount: a small always-on-top, click-through panel in a screen
//! corner, shown/hidden with a global hotkey (default Ctrl+Alt+F12). It only reads the JSON
//! that `sfs-mount --stats-out FILE` rewrites every 2 s, plus process RAM and free disk from
//! the OS; it never touches the game (no injection), so it works with any game that runs in
//! windowed or borderless-windowed mode (exclusive fullscreen draws over it).
//!
//! usage: sfs-overlay --stats FILE [--drive K:] [--corner tr|tl|br|bl] [--show]

#![windows_subsystem = "windows"]

use eframe::egui;
use global_hotkey::hotkey::{Code, HotKey, Modifiers};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

fn arg(name: &str) -> Option<String> {
    let a: Vec<String> = std::env::args().collect();
    a.iter().position(|x| x == name).and_then(|i| a.get(i + 1).cloned())
}

const W: f32 = 300.0;
const H: f32 = 340.0;
const MARGIN: f32 = 16.0;

/// One reading of the stats file.
#[derive(Clone, Default)]
struct Sample {
    at: Option<Instant>,
    served: u64,
    backend: u64,
    hit: u64,
    missed: u64,
    blocked_reads: u64,
    blocked_wall_ms: u64,
    blocked_max_us: u64,
    blocked_p95_us: u64,
    errors: u64,
    retries: u64,
    corrupt: u64,
    resident: u64,
    cap: u64,
    tunnels_down_total: u64,
}

impl Sample {
    fn from_json(v: &serde_json::Value) -> Self {
        let e = &v["engine"];
        let n = |x: &serde_json::Value| x.as_u64().unwrap_or(0);
        Sample {
            at: Some(Instant::now()),
            served: n(&e["bytes_served"]),
            backend: n(&e["backend_bytes"]),
            hit: n(&e["blocks_hit"]),
            missed: n(&e["blocks_missed"]),
            blocked_reads: n(&e["blocked_reads"]),
            blocked_wall_ms: n(&e["blocked_wall_ms"]),
            blocked_max_us: n(&e["blocked_max_us"]),
            blocked_p95_us: n(&e["blocked_p95_us"]),
            errors: n(&e["backend_errors"]),
            retries: n(&e["retries"]),
            corrupt: n(&e["corrupt_blocks"]),
            resident: n(&v["store"]["resident_bytes"]),
            cap: n(&v["store"]["max_bytes"]),
            tunnels_down_total: n(&v["http"]["tunnel_down"]),
        }
    }
}

struct App {
    stats: PathBuf,
    drive: String,
    visible: bool,
    _hk_mgr: GlobalHotKeyManager,
    hk_id: u32,
    cur: Sample,
    prev: Sample,
    file_mtime: Option<SystemTime>,
    last_poll: Instant,
    sys: sysinfo::System,
    disks: sysinfo::Disks,
    game_ram: Option<(String, u64)>,
    engine_ram: u64,
    free_disk: Option<u64>,
}

impl App {
    fn poll(&mut self) {
        if self.last_poll.elapsed() < Duration::from_millis(500) {
            return;
        }
        self.last_poll = Instant::now();
        let mtime = std::fs::metadata(&self.stats).and_then(|m| m.modified()).ok();
        if mtime.is_some() && mtime != self.file_mtime {
            if let Some(v) = std::fs::read(&self.stats).ok().and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok()) {
                self.file_mtime = mtime;
                self.prev = std::mem::replace(&mut self.cur, Sample::from_json(&v));
            }
        }
        // processes: whatever runs from the mounted drive is "the game"; sfs-mount is the engine
        self.sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
        let prefix = format!("{}\\", self.drive.trim_end_matches(['\\', '/']).to_uppercase());
        let mut best: Option<(String, u64)> = None;
        let mut eng = 0;
        for p in self.sys.processes().values() {
            let mem = p.memory();
            if p.name().to_string_lossy().eq_ignore_ascii_case("sfs-mount.exe") {
                eng += mem;
            } else if p.exe().map(|e| e.to_string_lossy().to_uppercase().starts_with(&prefix)).unwrap_or(false)
                && best.as_ref().map_or(true, |b| mem > b.1)
            {
                best = Some((p.name().to_string_lossy().trim_end_matches(".exe").to_string(), mem));
            }
        }
        self.game_ram = best;
        self.engine_ram = eng;
        self.disks.refresh();
        let cache_drive = self.stats.to_string_lossy().chars().next().map(|c| c.to_ascii_uppercase());
        self.free_disk = self
            .disks
            .iter()
            .find(|d| d.mount_point().to_string_lossy().chars().next().map(|c| c.to_ascii_uppercase()) == cache_drive)
            .map(|d| d.available_space());
    }
}

/// Sizes in binary units, labelled the way Windows Explorer labels them (GB = 2^30).
fn gb(b: u64) -> String {
    let g = b as f64 / (1u64 << 30) as f64;
    if g >= 10.0 {
        format!("{g:.1} GB")
    } else if g >= 1.0 {
        format!("{g:.2} GB")
    } else {
        format!("{:.0} MB", b as f64 / (1u64 << 20) as f64)
    }
}

fn secs(us: u64) -> String {
    if us >= 1_000_000 {
        format!("{:.1} s", us as f64 / 1e6)
    } else {
        format!("{} ms", us / 1000)
    }
}

impl eframe::App for App {
    fn clear_color(&self, _: &egui::Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 0.0] // fully transparent outside the panel
    }

    fn update(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        while let Ok(ev) = GlobalHotKeyEvent::receiver().try_recv() {
            if ev.id == self.hk_id && ev.state == HotKeyState::Pressed {
                self.visible = !self.visible;
            }
        }
        self.poll();
        ctx.request_repaint_after(Duration::from_millis(250));
        if !self.visible {
            return; // window stays, but draws nothing and lets clicks through
        }

        let c = &self.cur;
        let p = &self.prev;
        let dt = match (c.at, p.at) {
            (Some(a), Some(b)) if a > b => a.duration_since(b).as_secs_f64(),
            _ => 0.0,
        };
        let rate = |a: u64, b: u64| if dt > 0.0 { a.saturating_sub(b) as f64 / dt / 1e6 } else { 0.0 };
        let down = rate(c.backend, p.backend);
        let read = rate(c.served, p.served);
        let waiting = dt > 0.0 && c.blocked_wall_ms > p.blocked_wall_ms;
        let stale = self.file_mtime.and_then(|m| m.elapsed().ok()).map_or(true, |e| e > Duration::from_secs(6));
        let hit_pct = if c.hit + c.missed > 0 { 100.0 * c.hit as f64 / (c.hit + c.missed) as f64 } else { 100.0 };

        // colours: neutral text on a dark translucent panel; colour only for state
        let text = egui::Color32::from_rgb(232, 232, 232);
        let dim = egui::Color32::from_rgb(160, 160, 160);
        let ok = egui::Color32::from_rgb(110, 210, 130);
        let warn = egui::Color32::from_rgb(240, 190, 80);
        let bad = egui::Color32::from_rgb(240, 110, 100);

        let frame = egui::Frame::none()
            .fill(egui::Color32::from_rgba_unmultiplied(12, 12, 14, 200))
            .rounding(8.0)
            .inner_margin(egui::Margin::same(12.0));
        egui::CentralPanel::default().frame(egui::Frame::none()).show(ctx, |ui| {
            frame.show(ui, |ui| {
                ui.set_width(W - 24.0);
                ui.style_mut().spacing.item_spacing.y = 3.0;
                let row = |ui: &mut egui::Ui, k: &str, v: String, col: egui::Color32| {
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(k).color(dim).size(13.0));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(egui::RichText::new(v).color(col).size(14.0).monospace());
                        });
                    });
                };
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(format!("StreamFS · {}", self.drive)).color(text).size(15.0).strong());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let (t, col) = if stale { ("sin datos del motor", bad) } else { ("activo", ok) };
                        ui.label(egui::RichText::new(t).color(col).size(13.0));
                        // status dot (painted: the bundled fonts have no bullet glyph)
                        let (r, _) = ui.allocate_exact_size(egui::vec2(10.0, 10.0), egui::Sense::hover());
                        ui.painter().circle_filled(r.center(), 4.0, col);
                    });
                });
                ui.add_space(4.0);
                row(ui, "Bajando del servidor", format!("{down:.1} MB/s"), if down > 0.05 { warn } else { text });
                row(ui, "Juego leyendo", format!("{read:.1} MB/s"), text);
                row(ui, "Esperando datos ahora", if waiting { "sí".into() } else { "no".into() }, if waiting { warn } else { ok });
                row(ui, "Servido desde el PC", format!("{hit_pct:.1} %"), text);
                ui.add_space(4.0);
                row(ui, "Esperas (sesión)", format!("{}", c.blocked_reads), text);
                row(ui, "Espera típica 95% / máx", format!("{} / {}", secs(c.blocked_p95_us), secs(c.blocked_max_us)), text);
                row(ui, "Bajado (sesión)", gb(c.backend), text);
                row(ui, "Leído por el juego", gb(c.served), text);
                ui.add_space(4.0);
                row(ui, "Caché en disco", format!("{} / {}", gb(c.resident), gb(c.cap)), text);
                if let Some(f) = self.free_disk {
                    row(ui, "Libre en el disco", gb(f), text);
                }
                let game = self.game_ram.as_ref().map(|(n, m)| format!("{n} {}", gb(*m))).unwrap_or_else(|| "—".into());
                row(ui, "RAM juego", game, text);
                row(ui, "RAM motor", gb(self.engine_ram), text);
                // network failures are retried/raced by the engine; the game never sees them
                row(ui, "Fallos de red / reintentos", format!("{} / {}", c.errors, c.retries), text);
                if c.corrupt > 0 {
                    row(ui, "Bloques corruptos (descartados)", format!("{}", c.corrupt), bad);
                }
                if c.tunnels_down_total > p.tunnels_down_total {
                    ui.label(egui::RichText::new("reconectando un túnel…").color(warn).size(13.0));
                }
            });
        });
    }
}

fn main() -> eframe::Result<()> {
    let stats = PathBuf::from(arg("--stats").unwrap_or_else(|| "sfs-stats.json".into()));
    let drive = arg("--drive").unwrap_or_else(|| "K:".into());
    let corner = arg("--corner").unwrap_or_else(|| "tr".into());

    let mgr = GlobalHotKeyManager::new().expect("hotkey manager");
    let hk = HotKey::new(Some(Modifiers::CONTROL | Modifiers::ALT), Code::F12);
    mgr.register(hk).expect("Ctrl+Alt+F12 is already taken by another program");

    // place the window in a corner of the primary monitor (logical pixels)
    let (sw, sh) = primary_size();
    let x = if corner.contains('l') { MARGIN } else { sw - W - MARGIN };
    let y = if corner.contains('b') { sh - H - MARGIN - 48.0 } else { MARGIN };
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([W, H])
            .with_position([x, y])
            .with_decorations(false)
            .with_transparent(true)
            .with_always_on_top()
            .with_mouse_passthrough(true)
            .with_taskbar(false)
            .with_resizable(false)
            .with_title("StreamFS overlay"),
        ..Default::default()
    };
    let visible = std::env::args().any(|a| a == "--show");
    eframe::run_native(
        "StreamFS overlay",
        opts,
        Box::new(move |_cc| {
            Ok(Box::new(App {
                stats,
                drive,
                visible,
                _hk_mgr: mgr,
                hk_id: hk.id(),
                cur: Sample::default(),
                prev: Sample::default(),
                file_mtime: None,
                last_poll: Instant::now() - Duration::from_secs(1),
                sys: sysinfo::System::new(),
                disks: sysinfo::Disks::new_with_refreshed_list(),
                game_ram: None,
                engine_ram: 0,
                free_disk: None,
            }))
        }),
    )
}

#[cfg(windows)]
fn primary_size() -> (f32, f32) {
    extern "system" {
        fn GetSystemMetrics(i: i32) -> i32;
    }
    // SM_CXSCREEN / SM_CYSCREEN; the process is not DPI-aware here, so these are logical px
    unsafe { (GetSystemMetrics(0) as f32, GetSystemMetrics(1) as f32) }
}

#[cfg(not(windows))]
fn primary_size() -> (f32, f32) {
    (1920.0, 1080.0)
}
