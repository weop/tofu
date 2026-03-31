use fuzzy_matcher::FuzzyMatcher;
use smithay_client_toolkit::{
    reexports::calloop,
    compositor::{CompositorHandler, CompositorState},
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
        wlr_layer::{Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface},
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
use wayland_client::{Connection, QueueHandle};

// Font settings
const FONT_SIZE: f32 = 20.0;
const LINE_HEIGHT: u32 = 36;
const PADDING: u32 = 24;
const WINDOW_WIDTH: u32 = 800; // Fixed width centered on screen

fn parse_color(color_str: &str) -> u32 {
    let s = color_str.trim_start_matches('#');
    if s.len() == 6 {
        if let Ok(val) = u32::from_str_radix(s, 16) {
            return 0xff000000 | val; // Add full alpha
        }
    }
    0xff4488ff // Default blue
}

fn find_font_by_name(font_name: &str) -> Option<fontdue::Font> {
    // Try fc-match to find the font file
    let output = Command::new("fc-match")
        .args(["-f", "%{file}", font_name])
        .output()
        .ok()?;
    
    let path = String::from_utf8_lossy(&output.stdout);
    let path = path.trim();
    
    if path.is_empty() || path == "nil" || path.contains("dejavu") && font_name.to_lowercase().contains("geist") {
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
                if let Ok(font) = fontdue::Font::from_bytes(data, fontdue::FontSettings::default()) {
                    return font;
                }
            }
        }
        
        eprintln!("Warning: Could not find font '{}', using system default", spec);
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
    
    for arg in &args[1..] {
        if arg == "--drun" {
            drun_mode = true;
        } else if arg == "--invert" {
            invert_mode = true;
        } else if arg.starts_with("--color=") {
            accent_color = parse_color(&arg[8..]);
        } else if arg.starts_with("--font=") {
            font_spec = Some(arg[7..].to_string());
        }
    }
    
    let apps = if drun_mode {
        get_desktop_apps()
    } else {
        let mut input = String::new();
        io::stdin().read_to_string(&mut input).unwrap();
        input.lines().map(|s| (s.to_string(), s.to_string(), None)).filter(|(n, _, _)| !n.is_empty()).collect()
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

    let surface = compositor.create_surface(&qh);
    let layer = layer_shell.create_layer_surface(&qh, surface.clone(), Layer::Top, Some("tofu"), None);
    
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
    };
    
    app.filter();
    
    // Set up a timer for cursor blinking
    let timer = std::time::Duration::from_millis(500);
    let mut last_blink = std::time::Instant::now();
    
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
    if let Ok(output) = Command::new("fc-match").args(["-f", "%{file}", "monospace"]).output() {
        let path = String::from_utf8_lossy(&output.stdout);
        if !path.is_empty() && path != "nil" {
            if let Ok(data) = fs::read(path.trim()) {
                if let Ok(font) = fontdue::Font::from_bytes(data, fontdue::FontSettings::default()) {
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
                            if let Ok(font) = fontdue::Font::from_bytes(data, fontdue::FontSettings::default()) {
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
    
    let data_dirs = std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| 
        "/usr/local/share:/usr/share".to_string());
    let home = std::env::var("HOME").unwrap_or_default();
    
    let mut paths: Vec<String> = data_dirs.split(':').map(|s| format!("{}/applications", s)).collect();
    paths.insert(0, format!("{}/.local/share/applications", home));
    paths.push(format!("{}/.local/share/flatpak/exports/share/applications", home));
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
    let exec = exec.split_whitespace()
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
}

impl App {
    fn filter(&mut self) {
        let matcher = fuzzy_matcher::skim::SkimMatcherV2::default();
        let mut filtered: Vec<_> = self
            .items
            .iter()
            .filter_map(|item| {
                matcher.fuzzy_match(&item.0, &self.query).map(|s| (s, item.clone()))
            })
            .collect();
        filtered.sort_by(|a, b| b.0.cmp(&a.0));
        self.filtered = filtered.into_iter().take(10).collect();
        self.selected = self.selected.min(self.filtered.len().saturating_sub(1));
        self.needs_redraw = true;
    }
    
    fn draw(&mut self) {
        let scale = self.scale_factor.max(1) as u32;
        
        // Scaled dimensions
        let height = 500u32 * scale;
        let output_width = self.output_width * scale;
        let window_width = WINDOW_WIDTH * scale;
        let padding = PADDING * scale;
        let line_height = LINE_HEIGHT * scale;
        let input_box_height = 50u32 * scale;
        let corner_radius = 12u32 * scale;
        let font_size = FONT_SIZE * scale as f32;
        
        let margin_x = (output_width.saturating_sub(window_width)) / 2;
        let stride = output_width * 4;
        
        // Pre-calculate string widths
        let query_width = self.string_width_scaled(&self.query, font_size);
        let cursor_x = margin_x + padding + (12 * scale) + query_width;
        let cursor_char_width = self.string_width_scaled("m", font_size);
        
        let (buffer, mut canvas) = self.pool.create_buffer(output_width as i32, height as i32, stride as i32, wayland_client::protocol::wl_shm::Format::Argb8888).unwrap();
        
        // Clear background - transparent outside window area
        for chunk in canvas.chunks_exact_mut(4) {
            chunk.copy_from_slice(&[0x00, 0x00, 0x00, 0x00]);
        }
        
        // Draw main black background area (wraps input + results) with rounded corners
        let query_y = PADDING * scale;
        let results_height = (10u32 * line_height) + (10 * scale);
        let total_height = input_box_height + (10 * scale) + results_height;
        let container_top = query_y;
        let container_bottom = query_y + total_height;
        let container_left = margin_x;
        let container_right = margin_x + window_width;
        
        draw_rounded_rect(&mut canvas, output_width, height, margin_x, query_y, window_width, total_height, corner_radius, 0xff000000);
        
        // Draw input box background (slightly lighter than main bg) with rounded corners
        draw_rounded_rect(&mut canvas, output_width, height, margin_x + (8 * scale), query_y + (8 * scale), 
                         window_width - (16 * scale), input_box_height - (16 * scale), corner_radius / 2, 0xff1a1a1a);
        
        // Calculate vertical center of input box for text
        let text_baseline = query_y + (input_box_height / 2) + (font_size as u32 / 3);
        
        // Draw query text (vertically centered)
        draw_string_internal_scaled(&mut canvas, output_width, margin_x + padding + (12 * scale), text_baseline, 
                                   &self.query, 0xffffffff, &self.font, font_size);
        
        // Draw block cursor with accent color (blinking)
        if self.cursor_visible {
            draw_rect(&mut canvas, output_width, height, cursor_x, query_y + (12 * scale), cursor_char_width, 26 * scale, self.accent_color);
        }

        // Collect item names first to avoid borrow issues
        let item_names: Vec<(String, bool)> = self.filtered.iter().enumerate()
            .map(|(i, (_, item))| (item.0.clone(), i == self.selected))
            .collect();
        
        // Always assume 10 results for consistent fade calculation
        let max_results_for_fade = 10.0;
        
        // Draw items (centered on current output) with fade out and clipping
        let start_y = query_y + (70 * scale);
        let results_top = query_y + input_box_height + (10 * scale);
        let results_bottom = container_bottom - (10 * scale);
        
        for (i, (name, is_selected)) in item_names.iter().enumerate() {
            let y = start_y + i as u32 * line_height;
            let item_bottom = y + line_height - (4 * scale);
            
            // Skip if item is completely outside the container
            if y >= results_bottom || item_bottom <= results_top {
                continue;
            }
            
            // Clip to container bounds
            let draw_y = y.max(results_top);
            let draw_bottom = item_bottom.min(results_bottom);
            let draw_height = draw_bottom.saturating_sub(draw_y);
            if draw_height == 0 {
                continue;
            }
            
            // Calculate fade opacity - always assume 10 results max for consistent fade
            // Minimum opacity is 20% (0.2) so items never fully disappear
            let fade = 1.0 - (i as f32 / max_results_for_fade).powf(0.7);
            let fade = fade.clamp(0.2, 1.0);
            
            if *is_selected {
                if self.invert_mode {
                    // Inverted mode: black background, accent color text
                    let bg_color = 0xff000000u32;
                    draw_rect_clipped(&mut canvas, output_width, height, margin_x, draw_y, window_width, draw_height, 
                                     container_left, container_top, container_right, container_bottom, bg_color);
                    if y + (24 * scale) > results_top && y + (24 * scale) < results_bottom {
                        draw_string_internal_scaled(&mut canvas, output_width, margin_x + padding + (12 * scale), 
                                                   y + (24 * scale), name, self.accent_color, &self.font, font_size);
                    }
                } else {
                    // Normal mode: accent color background, white text
                    draw_rect_clipped(&mut canvas, output_width, height, margin_x, draw_y, window_width, draw_height, 
                                     container_left, container_top, container_right, container_bottom, self.accent_color);
                    if y + (24 * scale) > results_top && y + (24 * scale) < results_bottom {
                        draw_string_internal_scaled(&mut canvas, output_width, margin_x + padding + (12 * scale), 
                                                   y + (24 * scale), name, 0xffffffff, &self.font, font_size);
                    }
                }
            } else {
                // Unselected items: fade by blending with black background
                let bg_r = (0x20 as f32 * fade) as u8;
                let bg_g = (0x20 as f32 * fade) as u8;
                let bg_b = (0x20 as f32 * fade) as u8;
                let bg_color = 0xff000000 | ((bg_r as u32) << 16) | ((bg_g as u32) << 8) | (bg_b as u32);
                
                if fade > 0.05 {
                    draw_rect_clipped(&mut canvas, output_width, height, margin_x, draw_y, window_width, draw_height, 
                                     container_left, container_top, container_right, container_bottom, bg_color);
                    
                    if y + (24 * scale) > results_top && y + (24 * scale) < results_bottom {
                        let txt_r = (0xcc as f32 * fade) as u8;
                        let txt_g = (0xcc as f32 * fade) as u8;
                        let txt_b = (0xcc as f32 * fade) as u8;
                        let text_color = 0xff000000 | ((txt_r as u32) << 16) | ((txt_g as u32) << 8) | (txt_b as u32);
                        draw_string_internal_scaled(&mut canvas, output_width, margin_x + padding + (12 * scale), 
                                                   y + (24 * scale), name, text_color, &self.font, font_size);
                    }
                }
            }
        }
        
        buffer.attach_to(&self.surface).unwrap();
        self.surface.damage_buffer(0, 0, output_width as i32, height as i32);
        self.surface.commit();
    }
    
    fn string_width(&self, text: &str) -> u32 {
        self.string_width_scaled(text, FONT_SIZE)
    }
    
    fn string_width_scaled(&self, text: &str, font_size: f32) -> u32 {
        let mut width = 0u32;
        for c in text.chars() {
            let (metrics, _) = self.font.rasterize(c, font_size);
            width += metrics.advance_width as u32;
        }
        width
    }
    

    
    fn handle_key(&mut self, keysym: Keysym) {
        match keysym {
            Keysym::Escape => self.exit = true,
            Keysym::Return => {
                if let Some((_, item)) = self.filtered.get(self.selected) {
                    launch_app(&item.1);
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

fn launch_app(exec: &str) {
    if exec.starts_with("flatpak run") {
        let parts: Vec<_> = exec.split_whitespace().collect();
        if parts.len() >= 3 {
            let _ = Command::new("flatpak").args(&parts[2..]).spawn();
        }
    } else {
        let parts: Vec<_> = exec.split_whitespace().collect();
        if !parts.is_empty() {
            let mut cmd = Command::new(&parts[0]);
            if parts.len() > 1 {
                cmd.args(&parts[1..]);
            }
            let _ = cmd.spawn();
        }
    }
}

fn draw_rect(canvas: &mut [u8], width: u32, _height: u32, x: u32, y: u32, w: u32, h: u32, color: u32) {
    let bytes = color.to_le_bytes();
    for row in y..(y + h).min(500) {
        for col in x..(x + w).min(width) {
            let idx = ((row * width + col) * 4) as usize;
            if idx + 4 <= canvas.len() {
                canvas[idx..idx+4].copy_from_slice(&bytes);
            }
        }
    }
}

fn draw_rounded_rect(canvas: &mut [u8], width: u32, height: u32, x: u32, y: u32, w: u32, h: u32, radius: u32, color: u32) {
    let bytes = color.to_le_bytes();
    
    for row in y..(y + h).min(height) {
        for col in x..(x + w).min(width) {
            // Check if pixel is inside rounded corners
            let dx = if col < x + radius {
                radius - (col - x)
            } else if col >= x + w - radius {
                radius - (x + w - 1 - col)
            } else {
                0
            };
            
            let dy = if row < y + radius {
                radius - (row - y)
            } else if row >= y + h - radius {
                radius - (y + h - 1 - row)
            } else {
                0
            };
            
            // If in a corner region, check if inside the rounded arc
            let in_corner = dx > 0 && dy > 0;
            let draw_pixel = if in_corner {
                // Simple approximation: check if within radius
                (dx * dx + dy * dy) <= (radius * radius)
            } else {
                true
            };
            
            if draw_pixel {
                let idx = ((row * width + col) * 4) as usize;
                if idx + 4 <= canvas.len() {
                    canvas[idx..idx+4].copy_from_slice(&bytes);
                }
            }
        }
    }
}

fn draw_rect_clipped(canvas: &mut [u8], width: u32, _height: u32, x: u32, y: u32, w: u32, h: u32, clip_left: u32, clip_top: u32, clip_right: u32, clip_bottom: u32, color: u32) {
    let bytes = color.to_le_bytes();
    let start_x = x.max(clip_left);
    let end_x = (x + w).min(clip_right);
    let start_y = y.max(clip_top);
    let end_y = (y + h).min(clip_bottom);
    
    for row in start_y..end_y.min(500) {
        for col in start_x..end_x.min(width) {
            let idx = ((row * width + col) * 4) as usize;
            if idx + 4 <= canvas.len() {
                canvas[idx..idx+4].copy_from_slice(&bytes);
            }
        }
    }
}

fn draw_string_internal(canvas: &mut [u8], width: u32, x: u32, y: u32, text: &str, color: u32, font: &fontdue::Font) {
    draw_string_internal_scaled(canvas, width, x, y, text, color, font, FONT_SIZE);
}

fn draw_string_internal_scaled(canvas: &mut [u8], width: u32, x: u32, y: u32, text: &str, color: u32, font: &fontdue::Font, font_size: f32) {
    let bytes = color.to_le_bytes();
    let mut cx = x;
    
    for c in text.chars() {
        let (metrics, bitmap) = font.rasterize(c, font_size);
        
        let glyph_width = metrics.width;
        let glyph_height = metrics.height;
        let baseline = y as i32 - metrics.ymin;
        
        for gy in 0..glyph_height {
            for gx in 0..glyph_width {
                let bitmap_idx = gy * glyph_width + gx;
                if bitmap_idx < bitmap.len() {
                    let alpha = bitmap[bitmap_idx] as u32;
                    if alpha > 0 {
                        let px = cx + gx as u32;
                        let py = baseline as u32 - glyph_height as u32 + gy as u32;
                        
                        if px < width && py < 5000 { // Large enough for scaled height
                            let idx = ((py * width + px) * 4) as usize;
                            if idx + 4 <= canvas.len() {
                                // Alpha blend
                                let fg_r = bytes[0] as u32;
                                let fg_g = bytes[1] as u32;
                                let fg_b = bytes[2] as u32;
                                let bg_r = canvas[idx] as u32;
                                let bg_g = canvas[idx + 1] as u32;
                                let bg_b = canvas[idx + 2] as u32;
                                
                                let a = alpha;
                                let inv_a = 255 - alpha;
                                
                                canvas[idx] = ((fg_r * a + bg_r * inv_a) / 255) as u8;
                                canvas[idx + 1] = ((fg_g * a + bg_g * inv_a) / 255) as u8;
                                canvas[idx + 2] = ((fg_b * a + bg_b * inv_a) / 255) as u8;
                                canvas[idx + 3] = 0xff;
                            }
                        }
                    }
                }
            }
        }
        
        cx += metrics.advance_width as u32;
    }
}

impl CompositorHandler for App {
    fn scale_factor_changed(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _surface: &WlSurface, factor: i32) {
        if factor > 0 {
            self.scale_factor = factor;
            self.needs_redraw = true;
        }
    }
    fn frame(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _surface: &WlSurface, _time: u32) {}
    fn transform_changed(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _surface: &WlSurface, _transform: wayland_client::protocol::wl_output::Transform) {}
    fn surface_enter(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _surface: &WlSurface, _output: &WlOutput) {}
    fn surface_leave(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _surface: &WlSurface, _output: &WlOutput) {}
}

impl OutputHandler for App {
    fn output_state(&mut self) -> &mut OutputState { &mut self.output_state }
    fn new_output(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _output: WlOutput) {}
    fn update_output(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _output: WlOutput) {}
    fn output_destroyed(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _output: WlOutput) {}
}

impl SeatHandler for App {
    fn seat_state(&mut self) -> &mut SeatState { &mut self.seat_state }
    fn new_seat(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _seat: WlSeat) {}
    fn new_capability(&mut self, _conn: &Connection, qh: &QueueHandle<Self>, seat: WlSeat, cap: Capability) {
        if cap == Capability::Keyboard && self.keyboard.is_none() {
            self.keyboard = Some(self.seat_state.get_keyboard(qh, &seat, None).unwrap());
        }
    }
    fn remove_capability(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _seat: WlSeat, cap: Capability) {
        if cap == Capability::Keyboard && self.keyboard.is_some() {
            self.keyboard = None;
        }
    }
    fn remove_seat(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _seat: WlSeat) {}
}

impl KeyboardHandler for App {
    fn enter(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _keyboard: &WlKeyboard, _surface: &WlSurface, _serial: u32, _raw: &[u32], _keysyms: &[Keysym]) {}
    fn leave(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _keyboard: &WlKeyboard, _surface: &WlSurface, _serial: u32) {}
    fn press_key(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _keyboard: &WlKeyboard, _serial: u32, event: KeyEvent) {
        // Handle text input for printable characters
        if let Some(text) = event.utf8.as_ref() {
            if !text.is_empty() && !text.chars().next().map_or(false, |c| c.is_control()) {
                self.handle_text(text);
                return;
            }
        }
        self.handle_key(event.keysym);
    }
    fn release_key(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _keyboard: &WlKeyboard, _serial: u32, _event: KeyEvent) {}
    fn update_modifiers(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _keyboard: &WlKeyboard, _serial: u32, _modifiers: Modifiers, _layout: u32) {}
}

impl LayerShellHandler for App {
    fn closed(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _layer: &LayerSurface) {
        self.exit = true;
    }
    fn configure(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _layer: &LayerSurface, configure: smithay_client_toolkit::shell::wlr_layer::LayerSurfaceConfigure, _serial: u32) {
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
    fn shm_state(&mut self) -> &mut Shm { &mut self.shm }
}

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState { &mut self.registry_state }
    registry_handlers!(OutputState, SeatState);
}

delegate_compositor!(App);
delegate_output!(App);
delegate_seat!(App);
delegate_keyboard!(App);
delegate_layer!(App);
delegate_shm!(App);
delegate_registry!(App);
