use fuzzy_matcher::FuzzyMatcher;
use serde::{Deserialize, Serialize};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState, Region},
    delegate_compositor, delegate_keyboard, delegate_layer, delegate_output, delegate_registry,
    delegate_seat, delegate_shm,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers},
        Capability, SeatHandler, SeatState,
    },
    shell::{
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
        },
        WaylandSurface,
    },
    shm::{slot::SlotPool, Shm, ShmHandler},
};
use std::collections::HashMap;
use std::fs;
use std::io::{self, Read};
use std::path::Path;
use std::process::Command;
use wayland_client::protocol::{
    wl_keyboard::WlKeyboard, wl_output::WlOutput, wl_seat::WlSeat, wl_surface::WlSurface,
};
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols::ext::background_effect::v1::client::{
    ext_background_effect_manager_v1::{self, ExtBackgroundEffectManagerV1},
    ext_background_effect_surface_v1::{self, ExtBackgroundEffectSurfaceV1},
};

// Font settings
const FONT_SIZE: f32 = 20.0;
const LINE_HEIGHT: u32 = 36;
const PADDING: u32 = 24;
const INPUT_BOX_HEIGHT: u32 = 64; // Taller box -> more vertical padding around the query text
const TEXT_INSET: u32 = 24; // Horizontal inset (from panel edge) shared by query + result text
const RESULTS_GAP: u32 = 4; // Vertical gap between the input box and the results list
const WINDOW_WIDTH: u32 = 600; // Fixed width centered on screen

fn parse_color(color_str: &str) -> u32 {
    let s = color_str.trim_start_matches('#');
    if s.len() == 6 {
        if let Ok(val) = u32::from_str_radix(s, 16) {
            return 0xff000000 | val; // Add full alpha
        }
    }
    0xff4488ff // Default blue
}

// Frecency data structures
const MAX_LAUNCHES_PER_APP: usize = 20;
const FRECENCY_VERSION: u32 = 1;

#[derive(Serialize, Deserialize, Default)]
struct FrecencyData {
    version: u32,
    apps: HashMap<String, AppUsage>,
}

#[derive(Serialize, Deserialize, Clone)]
struct AppUsage {
    launches: Vec<u64>,
}

fn get_frecency_path() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    let data_dir =
        std::env::var("XDG_DATA_HOME").unwrap_or_else(|_| format!("{}/.local/share", home));
    std::path::PathBuf::from(data_dir)
        .join("tofu")
        .join("frecency.json")
}

fn load_frecency() -> FrecencyData {
    let path = get_frecency_path();
    if let Ok(content) = fs::read_to_string(&path) {
        if let Ok(data) = serde_json::from_str::<FrecencyData>(&content) {
            return data;
        }
    }
    FrecencyData {
        version: FRECENCY_VERSION,
        apps: HashMap::new(),
    }
}

fn save_frecency(data: &FrecencyData) {
    let path = get_frecency_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(data) {
        let _ = fs::write(&path, json);
    }
}

fn reset_frecency() -> bool {
    let path = get_frecency_path();
    if path.exists() {
        fs::remove_file(&path).is_ok()
    } else {
        true
    }
}

fn record_launch(app_name: &str) {
    let mut data = load_frecency();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let usage = data.apps.entry(app_name.to_string()).or_insert(AppUsage {
        launches: Vec::new(),
    });
    usage.launches.push(now);

    // Trim old entries, keep only the most recent
    if usage.launches.len() > MAX_LAUNCHES_PER_APP {
        usage.launches.sort_unstable();
        usage.launches.reverse();
        usage.launches.truncate(MAX_LAUNCHES_PER_APP);
    }

    save_frecency(&data);
}

fn calculate_frecency(app_name: &str, frecency_data: &FrecencyData) -> i64 {
    let usage = match frecency_data.apps.get(app_name) {
        Some(u) => u,
        None => return 0,
    };

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let hour = 3600u64;
    let day = 24 * hour;
    let week = 7 * day;
    let month = 30 * day;

    let mut score: i64 = 0;
    for &timestamp in &usage.launches {
        let age = now.saturating_sub(timestamp);
        let weight = if age < 4 * hour {
            100
        } else if age < day {
            70
        } else if age < week {
            50
        } else if age < month {
            30
        } else {
            10
        };
        score += weight;
    }

    score
}

fn find_font_by_name(font_name: &str) -> Option<fontdue::Font> {
    // Try fc-match to find the font file
    let output = Command::new("fc-match")
        .args(["-f", "%{file}", font_name])
        .output()
        .ok()?;

    let path = String::from_utf8_lossy(&output.stdout);
    let path = path.trim();

    if path.is_empty()
        || path == "nil"
        || path.contains("dejavu") && font_name.to_lowercase().contains("geist")
    {
        // fc-match returned default, try more specific search
        return None;
    }

    let data = fs::read(path).ok()?;
    fontdue::Font::from_bytes(data, fontdue::FontSettings::default()).ok()
}

fn load_font(font_spec: Option<&str>) -> fontdue::Font {
    if let Some(spec) = font_spec {
        // Try to find the specified font
        if let Some(font) = find_font_by_name(spec) {
            return font;
        }

        // Try parsing as direct path
        if Path::new(spec).exists() {
            if let Ok(data) = fs::read(spec) {
                if let Ok(font) = fontdue::Font::from_bytes(data, fontdue::FontSettings::default())
                {
                    return font;
                }
            }
        }

        eprintln!(
            "Warning: Could not find font '{}', using system default",
            spec
        );
    }

    load_system_font()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();

    // Parse arguments
    let mut drun_mode = false;
    let mut accent_color = 0xff4488ff; // Default blue
    let mut font_spec: Option<String> = None;
    let mut invert_mode = false;
    let mut reset_frecency_flag = false;
    let mut glass_mode = false;

    for arg in &args[1..] {
        if arg == "--drun" {
            drun_mode = true;
        } else if arg == "--invert" {
            invert_mode = true;
        } else if arg == "--glass" {
            glass_mode = true;
        } else if arg == "--reset" {
            reset_frecency_flag = true;
        } else if arg.starts_with("--color=") {
            accent_color = parse_color(&arg[8..]);
        } else if arg.starts_with("--font=") {
            font_spec = Some(arg[7..].to_string());
        }
    }

    // Handle --reset: clear frecency data and exit
    if reset_frecency_flag {
        if reset_frecency() {
            println!("Frecency data reset successfully");
            std::process::exit(0);
        } else {
            eprintln!("Failed to reset frecency data");
            std::process::exit(1);
        }
    }

    // Load frecency data for drun mode
    let frecency_data = if drun_mode {
        load_frecency()
    } else {
        FrecencyData::default()
    };

    let apps = if drun_mode {
        get_desktop_apps()
    } else {
        let mut input = String::new();
        io::stdin().read_to_string(&mut input).unwrap();
        input
            .lines()
            .map(|s| (s.to_string(), s.to_string(), None))
            .filter(|(n, _, _)| !n.is_empty())
            .collect()
    };

    if apps.is_empty() {
        eprintln!("No items provided");
        std::process::exit(1);
    }

    // Load font (specified or system default)
    let font = load_font(font_spec.as_deref());

    let conn = Connection::connect_to_env().unwrap();
    let (globals, mut event_queue) = wayland_client::globals::registry_queue_init(&conn).unwrap();
    let qh = event_queue.handle();

    let compositor = CompositorState::bind(&globals, &qh).unwrap();
    let layer_shell = LayerShell::bind(&globals, &qh).unwrap();
    let shm = Shm::bind(&globals, &qh).unwrap();
    let output_state = OutputState::new(&globals, &qh);
    let seat_state = SeatState::new(&globals, &qh);

    // Background-effect (blur) manager - present on compositors like niri 26.04+.
    // This lets us scope blur to just the rounded panel; a niri layer-rule alone would
    // blur the entire (mostly transparent) full-width layer surface.
    let bg_effect_mgr = globals
        .bind::<ExtBackgroundEffectManagerV1, _, _>(&qh, 1..=1, ())
        .ok();

    let surface = compositor.create_surface(&qh);
    let layer =
        layer_shell.create_layer_surface(&qh, surface.clone(), Layer::Top, Some("tofu"), None);

    let bg_effect = if glass_mode {
        bg_effect_mgr
            .as_ref()
            .map(|m| m.get_background_effect(&surface, &qh, ()))
    } else {
        None
    };

    // Anchor to top of current output - compositor will place on output with keyboard focus
    layer.set_anchor(Anchor::TOP | Anchor::LEFT | Anchor::RIGHT);
    layer.set_size(0, 500); // 0 width = full width, we'll draw centered
    layer.set_exclusive_zone(0);
    layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
    layer.commit();

    let pool = SlotPool::new(2560 * 500 * 4, &shm).unwrap();

    let mut app = App {
        items: apps,
        filtered: Vec::new(),
        query: String::new(),
        selected: 0,
        exit: false,
        pool,
        layer,
        surface,
        compositor,
        glass: glass_mode,
        bg_effect,
        blur_set: false,
        blur_region: None,
        output_state,
        seat_state,
        keyboard: None,
        shm,
        registry_state: RegistryState::new(&globals),
        configured: false,
        font,
        needs_redraw: true,
        output_width: 1920,
        scale_factor: 1,
        accent_color,
        invert_mode,
        cursor_visible: true,
        frecency_data,
        drun_mode,
    };

    app.filter();

    while !app.exit {
        if app.needs_redraw && app.configured {
            app.draw();
            app.needs_redraw = false;
        }

        match event_queue.blocking_dispatch(&mut app) {
            Ok(_) => {}
            Err(e) => eprintln!("Wayland error: {}", e),
        }
    }
}

fn load_system_font() -> fontdue::Font {
    // Try common system fonts - expanded list
    let font_paths = [
        // DejaVu
        "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
        "/usr/share/fonts/TTF/DejaVuSansMono.ttf",
        "/usr/share/fonts/dejavu/DejaVuSansMono.ttf",
        // Liberation
        "/usr/share/fonts/truetype/liberation/LiberationMono-Regular.ttf",
        "/usr/share/fonts/liberation/LiberationMono-Regular.ttf",
        "/usr/share/fonts/TTF/LiberationMono-Regular.ttf",
        // Noto
        "/usr/share/fonts/truetype/noto/NotoSansMono-Regular.ttf",
        "/usr/share/fonts/noto/NotoSansMono-Regular.ttf",
        "/usr/share/fonts/TTF/NotoSansMono-Regular.ttf",
        // Hack
        "/usr/share/fonts/truetype/hack/Hack-Regular.ttf",
        "/usr/share/fonts/hack/Hack-Regular.ttf",
        "/usr/share/fonts/TTF/Hack-Regular.ttf",
        // Fira
        "/usr/share/fonts/opentype/fira/FiraMono-Regular.otf",
        "/usr/share/fonts/opentype/fira/FiraMono-Medium.otf",
        "/usr/share/fonts/TTF/FiraMono-Regular.ttf",
        // Ubuntu
        "/usr/share/fonts/truetype/ubuntu/UbuntuMono-R.ttf",
        "/usr/share/fonts/ubuntu/UbuntuMono-R.ttf",
        // Source Code Pro
        "/usr/share/fonts/opentype/source-code-pro/SourceCodePro-Regular.otf",
        "/usr/share/fonts/adobe-source-code-pro/SourceCodePro-Regular.otf",
        // Inconsolata
        "/usr/share/fonts/truetype/inconsolata/Inconsolata-Regular.ttf",
        // Cascadia
        "/usr/share/fonts/truetype/cascadia/CascadiaMono.ttf",
        "/usr/share/fonts/cascadia/CascadiaMono.ttf",
        // JetBrains
        "/usr/share/fonts/truetype/jetbrains/JetBrainsMono-Regular.ttf",
        "/usr/share/fonts/jetbrains/JetBrainsMono-Regular.ttf",
    ];

    for path in &font_paths {
        if let Ok(data) = fs::read(path) {
            if let Ok(font) = fontdue::Font::from_bytes(data, fontdue::FontSettings::default()) {
                return font;
            }
        }
    }

    // Try to find any monospace font using fontconfig
    if let Ok(output) = Command::new("fc-match")
        .args(["-f", "%{file}", "monospace"])
        .output()
    {
        let path = String::from_utf8_lossy(&output.stdout);
        if !path.is_empty() && path != "nil" {
            if let Ok(data) = fs::read(path.trim()) {
                if let Ok(font) = fontdue::Font::from_bytes(data, fontdue::FontSettings::default())
                {
                    return font;
                }
            }
        }
    }

    // Last resort: try any ttf/otf in common directories
    let font_dirs = [
        "/usr/share/fonts/truetype",
        "/usr/share/fonts/TTF",
        "/usr/share/fonts/opentype",
    ];

    for dir in &font_dirs {
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if let Some(ext) = path.extension() {
                    if ext == "ttf" || ext == "otf" {
                        if let Ok(data) = fs::read(&path) {
                            if let Ok(font) =
                                fontdue::Font::from_bytes(data, fontdue::FontSettings::default())
                            {
                                return font;
                            }
                        }
                    }
                }
            }
        }
    }

    panic!("No usable font found on system");
}

type AppEntry = (String, String, Option<String>);

fn get_desktop_apps() -> Vec<AppEntry> {
    let mut apps = HashMap::new();

    let data_dirs = std::env::var("XDG_DATA_DIRS")
        .unwrap_or_else(|_| "/usr/local/share:/usr/share".to_string());
    let home = std::env::var("HOME").unwrap_or_default();

    let mut paths: Vec<String> = data_dirs
        .split(':')
        .map(|s| format!("{}/applications", s))
        .collect();
    paths.insert(0, format!("{}/.local/share/applications", home));
    paths.push(format!(
        "{}/.local/share/flatpak/exports/share/applications",
        home
    ));
    paths.push("/var/lib/flatpak/exports/share/applications".to_string());

    for path in paths {
        if let Ok(entries) = fs::read_dir(&path) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().map_or(false, |e| e == "desktop") {
                    if let Some(app) = parse_desktop_file(&path) {
                        apps.entry(app.1.clone()).or_insert(app);
                    }
                }
            }
        }
    }

    let mut result: Vec<_> = apps.into_values().collect();
    result.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));
    result
}

fn parse_desktop_file(path: &Path) -> Option<AppEntry> {
    let content = fs::read_to_string(path).ok()?;

    let mut name = None;
    let mut exec = None;
    let mut icon = None;
    let mut no_display = false;
    let mut in_entry = false;

    for line in content.lines() {
        if line.starts_with("[Desktop Entry]") {
            in_entry = true;
        } else if line.starts_with('[') {
            in_entry = false;
        } else if in_entry {
            if let Some((key, val)) = line.split_once('=') {
                match key {
                    "Name" => name = Some(val.to_string()),
                    "Exec" => exec = Some(val.to_string()),
                    "Icon" => icon = Some(val.to_string()),
                    "NoDisplay" => no_display = val == "true",
                    _ => {}
                }
            }
        }
    }

    if no_display || name.is_none() || exec.is_none() {
        return None;
    }

    let name = name.unwrap();
    let exec = exec.unwrap();
    let exec = exec
        .split_whitespace()
        .filter(|s| !s.starts_with('%'))
        .collect::<Vec<_>>()
        .join(" ");

    if exec.is_empty() {
        return None;
    }

    Some((name, exec, icon))
}

struct App {
    items: Vec<AppEntry>,
    filtered: Vec<(i64, AppEntry)>,
    query: String,
    selected: usize,
    exit: bool,
    pool: SlotPool,
    #[allow(dead_code)]
    layer: LayerSurface,
    surface: WlSurface,
    compositor: CompositorState,
    glass: bool,
    bg_effect: Option<ExtBackgroundEffectSurfaceV1>,
    blur_set: bool,
    blur_region: Option<Region>,
    output_state: OutputState,
    seat_state: SeatState,
    keyboard: Option<WlKeyboard>,
    shm: Shm,
    registry_state: RegistryState,
    configured: bool,
    font: fontdue::Font,
    needs_redraw: bool,
    output_width: u32,
    scale_factor: i32,
    accent_color: u32,
    invert_mode: bool,
    cursor_visible: bool,
    frecency_data: FrecencyData,
    drun_mode: bool,
}

impl App {
    fn filter(&mut self) {
        let matcher = fuzzy_matcher::skim::SkimMatcherV2::default();
        let mut filtered: Vec<_> = self
            .items
            .iter()
            .filter_map(|item| {
                matcher
                    .fuzzy_match(&item.0, &self.query)
                    .map(|fuzzy_score| {
                        // Calculate frecency boost (only in drun mode)
                        let frecency_score = if self.drun_mode {
                            calculate_frecency(&item.0, &self.frecency_data)
                        } else {
                            0
                        };
                        // Combined score: frecency has significant weight but doesn't completely override fuzzy
                        let combined_score = fuzzy_score + (frecency_score * 10);
                        (combined_score, item.clone())
                    })
            })
            .collect();
        filtered.sort_by(|a, b| b.0.cmp(&a.0));
        self.filtered = filtered.into_iter().take(10).collect();
        self.selected = self.selected.min(self.filtered.len().saturating_sub(1));
        self.needs_redraw = true;
    }

    fn draw(&mut self) {
        let _t0 = std::time::Instant::now();
        let glass = self.glass;
        let sc = self.scale_factor.max(1) as u32; // device scale
        let scale = sc as f32 * if glass { 1.5 } else { 1.0 }; // glass: 1.5x supersample
        let px = |v: u32| (v as f32 * scale).round() as u32;
        let fs = FONT_SIZE * scale;

        // Render (supersampled) vs device dimensions.
        let (w, h) = (px(self.output_width), px(500));
        let (dev_w, dev_h) = (self.output_width * sc, 500 * sc);

        // Layout, in render pixels.
        let win = px(WINDOW_WIDTH);
        let mx = w.saturating_sub(win) / 2;
        let tx = mx + px(TEXT_INSET); // shared text left edge (query + results)
        let lh = px(LINE_HEIGHT);
        let ibh = px(INPUT_BOX_HEIGHT);
        let qy = px(PADDING);
        let gap = px(RESULTS_GAP);
        let total = ibh + gap + 10 * lh + px(10);
        let cbot = qy + total;
        let clip = (mx, qy, mx + win, cbot);
        let cont = if glass {
            premultiply(0x80_0a0a0a)
        } else {
            0xff000000
        };
        let inbox = if glass {
            premultiply(0x99_1a1a1a)
        } else {
            0xff1a1a1a
        };
        let (accent, invert, cursor_on) =
            (self.accent_color, self.invert_mode, self.cursor_visible);

        // Snapshot items + query so rendering borrows nothing else from self.
        let query = self.query.clone();
        let items: Vec<(String, bool)> = self
            .filtered
            .iter()
            .enumerate()
            .map(|(i, (_, it))| (it.0.clone(), i == self.selected))
            .collect();

        let (buffer, mut dev) = self
            .pool
            .create_buffer(
                dev_w as i32,
                dev_h as i32,
                (dev_w * 4) as i32,
                wayland_client::protocol::wl_shm::Format::Argb8888,
            )
            .unwrap();
        let mut render = vec![0u8; (w * h * 4) as usize];

        {
            let mut s = Surf {
                buf: &mut render,
                w,
                h,
                font: &self.font,
                fs,
            };
            s.rrect(mx, qy, win, total, px(12), cont); // panel
            s.rrect(
                mx + px(8),
                qy + px(8),
                win - px(16),
                ibh - px(16),
                px(6),
                inbox,
            ); // input box
            s.text(tx, qy + ibh / 2 + fs as u32 / 3, &query, 0xffffffff, 1.0);
            if cursor_on {
                let ch = px(26);
                let cw = s.text_w("m");
                let qw = s.text_w(&query);
                s.rect(
                    tx + qw,
                    qy + ibh.saturating_sub(ch) / 2,
                    cw,
                    ch,
                    clip,
                    accent,
                );
            }
            let rtop = qy + ibh + gap;
            let rbot = cbot - px(10);
            for (i, (name, sel)) in items.iter().enumerate() {
                let y = rtop + px(4) + i as u32 * lh;
                let ib = y + lh - px(4);
                if y >= rbot || ib <= rtop {
                    continue;
                }
                let (dy, dh) = (y.max(rtop), ib.min(rbot).saturating_sub(y.max(rtop)));
                if dh == 0 {
                    continue;
                }
                let fade = 1.0 - (i as f32 / 10.0).powf(0.7);
                let fade = if glass {
                    fade.clamp(0.05, 1.0)
                } else {
                    fade.clamp(0.2, 1.0)
                };
                let ty = y + px(24);
                let vis = ty > rtop && ty < rbot;
                let (row, rw) = (mx + px(8), win - px(16));
                if *sel {
                    if glass {
                        let c = if invert { accent } else { 0xffffffff };
                        if vis {
                            s.text(tx, ty, name, c, 1.0);
                        }
                    } else if invert {
                        s.rect(row, dy, rw, dh, clip, 0xff000000);
                        if vis {
                            s.text(tx, ty, name, accent, 1.0);
                        }
                    } else {
                        s.rect(row, dy, rw, dh, clip, accent);
                        if vis {
                            s.text(tx, ty, name, 0xffffffff, 1.0);
                        }
                    }
                } else if fade > 0.05 {
                    if glass {
                        if vis {
                            s.text(tx, ty, name, 0xcccccc, fade);
                        }
                    } else {
                        let g = (0x20 as f32 * fade) as u32;
                        s.rect(row, dy, rw, dh, clip, 0xff000000 | g << 16 | g << 8 | g);
                        if vis {
                            let v = (0xcc as f32 * fade) as u32;
                            s.text(tx, ty, name, 0xff000000 | v << 16 | v << 8 | v, 1.0);
                        }
                    }
                }
            }
        }

        // Scope the blur to the rounded panel only (logical surface coords).
        if glass && !self.blur_set {
            if let Some(eff) = self.bg_effect.as_ref() {
                if let Ok(reg) = Region::new(&self.compositor) {
                    let lmx = self.output_width.saturating_sub(WINDOW_WIDTH) / 2;
                    let tl = INPUT_BOX_HEIGHT + RESULTS_GAP + (10 * LINE_HEIGHT + 10);
                    add_rounded_region(
                        &reg,
                        lmx as i32,
                        PADDING as i32,
                        WINDOW_WIDTH as i32,
                        tl as i32,
                        12,
                    );
                    eff.set_blur_region(Some(reg.wl_region()));
                    self.blur_region = Some(reg);
                    self.blur_set = true;
                }
            }
        }

        // Only the centered panel has content; resample just its device-space bounding box.
        let m = 4 * sc;
        let bx = self.output_width.saturating_sub(WINDOW_WIDTH) / 2 * sc;
        let ph = (INPUT_BOX_HEIGHT + RESULTS_GAP + 10 * LINE_HEIGHT + 10) * sc;
        let rect = (
            bx.saturating_sub(m),
            (PADDING * sc).saturating_sub(m),
            bx + WINDOW_WIDTH * sc + m,
            PADDING * sc + ph + m,
        );
        resample(&render, &mut dev, w, h, dev_w, dev_h, rect);
        eprintln!(
            "BENCH glass={} render={}x{} us={}",
            glass,
            w,
            h,
            _t0.elapsed().as_micros()
        );
        buffer.attach_to(&self.surface).unwrap();
        self.surface.damage_buffer(0, 0, dev_w as i32, dev_h as i32);
        self.surface.commit();
    }

    fn handle_key(&mut self, keysym: Keysym) {
        match keysym {
            Keysym::Escape => self.exit = true,
            Keysym::Return => {
                if let Some((_, item)) = self.filtered.get(self.selected) {
                    let app_name = item.0.clone();
                    let success = launch_app(&item.1);
                    // Only record launch if spawn succeeded (app exists) and in drun mode
                    if success && self.drun_mode {
                        record_launch(&app_name);
                    }
                }
                self.exit = true;
            }
            Keysym::Up => {
                if self.selected > 0 {
                    self.selected -= 1;
                    self.needs_redraw = true;
                }
            }
            Keysym::Down => {
                if self.selected + 1 < self.filtered.len() {
                    self.selected += 1;
                    self.needs_redraw = true;
                }
            }
            Keysym::BackSpace => {
                if !self.query.is_empty() {
                    self.query.pop();
                    self.filter();
                }
            }
            _ => {}
        }
    }

    fn handle_text(&mut self, text: &str) {
        self.query.push_str(text);
        self.filter();
    }
}

fn launch_app(exec: &str) -> bool {
    if exec.starts_with("flatpak run") {
        let parts: Vec<_> = exec.split_whitespace().collect();
        if parts.len() >= 3 {
            return Command::new("flatpak").args(&parts[2..]).spawn().is_ok();
        }
        false
    } else {
        let parts: Vec<_> = exec.split_whitespace().collect();
        if !parts.is_empty() {
            let mut cmd = Command::new(&parts[0]);
            if parts.len() > 1 {
                cmd.args(&parts[1..]);
            }
            return cmd.spawn().is_ok();
        }
        false
    }
}

// Resolve the supersampled render buffer into the device buffer. Identity-copies when sizes
// match (non-glass); otherwise averages a 2x2 supersample block per device pixel (cheap
// integer math) which antialiases the glass 1.5x frame without per-pixel float weighting.
fn resample(
    src: &[u8],
    dst: &mut [u8],
    sw: u32,
    sh: u32,
    dw: u32,
    dh: u32,
    rect: (u32, u32, u32, u32),
) {
    if sw == dw && sh == dh {
        dst.copy_from_slice(src);
        return;
    }
    dst.fill(0); // everything outside the panel rect is transparent
    let (x0, y0, x1, y1) = rect;
    for dy in y0..y1.min(dh) {
        let (sy, sy1) = (dy * sh / dh, (dy * sh / dh + 1).min(sh - 1));
        for dx in x0..x1.min(dw) {
            let (sx, sx1) = (dx * sw / dw, (dx * sw / dw + 1).min(sw - 1));
            let o = ((dy * dw + dx) * 4) as usize;
            for c in 0..4 {
                let s = src[((sy * sw + sx) * 4) as usize + c] as u32
                    + src[((sy * sw + sx1) * 4) as usize + c] as u32
                    + src[((sy1 * sw + sx) * 4) as usize + c] as u32
                    + src[((sy1 * sw + sx1) * 4) as usize + c] as u32;
                dst[o + c] = (s / 4) as u8;
            }
        }
    }
}

// Convert a straight-alpha 0xAARRGGBB color to premultiplied alpha (wl_shm Argb8888).
fn premultiply(color: u32) -> u32 {
    let a = (color >> 24) & 0xff;
    let r = ((color >> 16) & 0xff) * a / 255;
    let g = ((color >> 8) & 0xff) * a / 255;
    let b = (color & 0xff) * a / 255;
    (a << 24) | (r << 16) | (g << 8) | b
}

// Build a rounded-rectangle blur region from horizontal spans (matching draw_rounded_rect's
// corner arc) so the blur follows the panel's rounded corners instead of a hard rectangle.
fn add_rounded_region(region: &Region, x: i32, y: i32, w: i32, h: i32, r: i32) {
    region.add(x, y + r, w, h - 2 * r);
    for i in 0..r {
        let dy = r - i;
        let inset = r - ((r * r - dy * dy) as f64).sqrt() as i32;
        let rw = w - 2 * inset;
        region.add(x + inset, y + i, rw, 1);
        region.add(x + inset, y + h - 1 - i, rw, 1);
    }
}

// A draw target: a pixel buffer (premultiplied Argb8888) plus the font for glyph rasterization.
struct Surf<'a> {
    buf: &'a mut [u8],
    w: u32,
    h: u32,
    font: &'a fontdue::Font,
    fs: f32,
}

impl Surf<'_> {
    // Filled rect (overwrite), clipped to (left, top, right, bottom).
    fn rect(&mut self, x: u32, y: u32, w: u32, h: u32, clip: (u32, u32, u32, u32), color: u32) {
        let b = color.to_le_bytes();
        for row in y.max(clip.1)..(y + h).min(clip.3).min(self.h) {
            for col in x.max(clip.0)..(x + w).min(clip.2).min(self.w) {
                let i = ((row * self.w + col) * 4) as usize;
                if i + 4 <= self.buf.len() {
                    self.buf[i..i + 4].copy_from_slice(&b);
                }
            }
        }
    }

    // Rounded rect: interior overwrites; corner arcs composite for antialiased edges (fix 2).
    fn rrect(&mut self, x: u32, y: u32, w: u32, h: u32, rad: u32, color: u32) {
        let b = color.to_le_bytes();
        for row in y..(y + h).min(self.h) {
            let cy = rad > 0 && (row < y + rad || row >= y + h - rad);
            for col in x..(x + w).min(self.w) {
                let i = ((row * self.w + col) * 4) as usize;
                if i + 4 > self.buf.len() {
                    continue;
                }
                let cx = rad > 0 && (col < x + rad || col >= x + w - rad);
                let cov = if cx && cy {
                    let (pc, pr) = (col as f32 + 0.5, row as f32 + 0.5);
                    let nx = pc.clamp((x + rad) as f32, (x + w - rad) as f32);
                    let ny = pr.clamp((y + rad) as f32, (y + h - rad) as f32);
                    (rad as f32 + 0.5 - ((pc - nx).powi(2) + (pr - ny).powi(2)).sqrt())
                        .clamp(0.0, 1.0)
                } else {
                    1.0
                };
                if cov >= 1.0 {
                    self.buf[i..i + 4].copy_from_slice(&b);
                } else if cov > 0.0 {
                    let sa = b[3] as f32 * cov / 255.0;
                    for c in 0..4 {
                        self.buf[i + c] =
                            (b[c] as f32 * cov + self.buf[i + c] as f32 * (1.0 - sa)) as u8;
                    }
                }
            }
        }
    }

    fn text_w(&self, s: &str) -> u32 {
        s.chars()
            .map(|c| self.font.rasterize(c, self.fs).0.advance_width as u32)
            .sum()
    }

    // Text via premultiplied source-over with an opacity factor.
    fn text(&mut self, x: u32, y: u32, s: &str, color: u32, op: f32) {
        let col = color.to_le_bytes();
        let mut cx = x;
        for c in s.chars() {
            let (m, bmp) = self.font.rasterize(c, self.fs);
            let (gw, gh, base, adv) =
                (m.width, m.height, y as i32 - m.ymin, m.advance_width as u32);
            for gy in 0..gh {
                let py = base - gh as i32 + gy as i32;
                if py < 0 || py as u32 >= self.h {
                    continue;
                }
                for gx in 0..gw {
                    let a = bmp[gy * gw + gx] as f32 / 255.0;
                    let pxp = cx + gx as u32;
                    if a == 0.0 || pxp >= self.w {
                        continue;
                    }
                    let i = ((py as u32 * self.w + pxp) * 4) as usize;
                    if i + 4 > self.buf.len() {
                        continue;
                    }
                    let (sa, inv) = (a * op, 1.0 - a * op);
                    for ch in 0..3 {
                        self.buf[i + ch] =
                            (col[ch] as f32 * sa + self.buf[i + ch] as f32 * inv) as u8;
                    }
                    self.buf[i + 3] = (255.0 * sa + self.buf[i + 3] as f32 * inv) as u8;
                }
            }
            cx += adv;
        }
    }
}

impl CompositorHandler for App {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &WlSurface,
        factor: i32,
    ) {
        if factor > 0 {
            self.scale_factor = factor;
            self.needs_redraw = true;
        }
    }
    fn frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &WlSurface,
        _time: u32,
    ) {
    }
    fn transform_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &WlSurface,
        _transform: wayland_client::protocol::wl_output::Transform,
    ) {
    }
    fn surface_enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &WlSurface,
        _output: &WlOutput,
    ) {
    }
    fn surface_leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &WlSurface,
        _output: &WlOutput,
    ) {
    }
}

impl OutputHandler for App {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }
    fn new_output(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _output: WlOutput) {}
    fn update_output(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _output: WlOutput) {}
    fn output_destroyed(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _output: WlOutput) {
    }
}

impl SeatHandler for App {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }
    fn new_seat(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _seat: WlSeat) {}
    fn new_capability(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        seat: WlSeat,
        cap: Capability,
    ) {
        if cap == Capability::Keyboard && self.keyboard.is_none() {
            self.keyboard = Some(self.seat_state.get_keyboard(qh, &seat, None).unwrap());
        }
    }
    fn remove_capability(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _seat: WlSeat,
        cap: Capability,
    ) {
        if cap == Capability::Keyboard && self.keyboard.is_some() {
            self.keyboard = None;
        }
    }
    fn remove_seat(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _seat: WlSeat) {}
}

impl KeyboardHandler for App {
    fn enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &WlKeyboard,
        _surface: &WlSurface,
        _serial: u32,
        _raw: &[u32],
        _keysyms: &[Keysym],
    ) {
    }
    fn leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &WlKeyboard,
        _surface: &WlSurface,
        _serial: u32,
    ) {
        self.exit = true;
    }
    fn press_key(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &WlKeyboard,
        _serial: u32,
        event: KeyEvent,
    ) {
        // Handle text input for printable characters
        if let Some(text) = event.utf8.as_ref() {
            if !text.is_empty() && !text.chars().next().map_or(false, |c| c.is_control()) {
                self.handle_text(text);
                return;
            }
        }
        self.handle_key(event.keysym);
    }
    fn release_key(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &WlKeyboard,
        _serial: u32,
        _event: KeyEvent,
    ) {
    }
    fn update_modifiers(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &WlKeyboard,
        _serial: u32,
        _modifiers: Modifiers,
        _layout: u32,
    ) {
    }
}

impl LayerShellHandler for App {
    fn closed(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _layer: &LayerSurface) {
        self.exit = true;
    }
    fn configure(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _layer: &LayerSurface,
        configure: smithay_client_toolkit::shell::wlr_layer::LayerSurfaceConfigure,
        _serial: u32,
    ) {
        // Update output width from configure event (this is in surface coordinates)
        if configure.new_size.0 > 0 {
            self.output_width = configure.new_size.0 as u32;
        }

        // Set the buffer scale to match output scale for HiDPI
        if self.scale_factor > 1 {
            self.surface.set_buffer_scale(self.scale_factor);
        }

        self.configured = true;
        self.needs_redraw = true;
    }
}

impl ShmHandler for App {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers!(OutputState, SeatState);
}

impl Dispatch<ExtBackgroundEffectManagerV1, ()> for App {
    fn event(
        _: &mut Self,
        _: &ExtBackgroundEffectManagerV1,
        _: ext_background_effect_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtBackgroundEffectSurfaceV1, ()> for App {
    fn event(
        _: &mut Self,
        _: &ExtBackgroundEffectSurfaceV1,
        _: ext_background_effect_surface_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

delegate_compositor!(App);
delegate_output!(App);
delegate_seat!(App);
delegate_keyboard!(App);
delegate_layer!(App);
delegate_shm!(App);
delegate_registry!(App);
