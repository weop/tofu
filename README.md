# TOFU

An ultra-minimalist application launcher for Wayland, inspired by [rofi](https://github.com/davatorium/rofi) and [dmenu](https://tools.suckless.org/dmenu/).

## Features

- **Tiny**: ~750 lines of Rust, ~1MB binary
- **Fast**: Native Wayland layer-shell, no X11 overhead
- **Fuzzy matching**: Skim's algorithm for fast filtering
- **Beautiful**: Fade-out results, rounded corners, customizable accent colors
- **Flexible**: dmenu mode or .desktop app launcher

## Installation

```bash
just install
```

Requires `~/.local/bin` in your PATH.

## Usage

### Basic Modes

**dmenu mode** (read from stdin):
```bash
echo -e "firefox\nterminal\nnvim" | tofu
ls /usr/bin | tofu | xargs swaymsg exec --
```

**App launcher mode** (read .desktop files):
```bash
tofu --drun
```

### Command Line Options

| Option | Description | Example |
|--------|-------------|---------|
| `--drun` | Launch app mode (read .desktop files) | `tofu --drun` |
| `--color="#RRGGBB"` | Set accent color (cursor + selection) | `tofu --drun --color="#b42400"` |
| `--font="PATTERN"` | Set font (fc-match pattern or path) | `tofu --drun --font="Geist Mono"` |
| `--invert` | Inverted selection style (accent text, black bg) | `tofu --drun --invert` |

### Examples

```bash
# Default blue theme
tofu --drun

# Orange/red theme with Geist Mono font
tofu --drun --color="#b42400" --font="Geist Mono"

# Green theme with custom font file
tofu --drun --color="#009B67" --font="/usr/share/fonts/TTF/Hack-Regular.ttf"

# Inverted mode (accent text color, black background for selected)
tofu --drun --color="#b42400" --invert

# In Sway config
set $menu tofu --drun --color="#4488ff" --font="Geist Mono"
bindsym $mod+space exec $menu

# In Niri config
binds {
    Mod+Space { spawn "tofu" "--drun" "--color=#009B67"; }
}
```

## Key Bindings

| Key | Action |
|-----|--------|
| Type | Filter apps |
| ↑ / Down | Navigate selection |
| Enter | Launch selected app |
| Esc | Cancel/exit |
| Backspace | Delete character |

## Visual Design

- **Centered**: Appears on the output with keyboard focus
- **Rounded corners**: 12px radius on main container
- **Fade effect**: Lower results fade to black (minimum 20% opacity)
- **Block cursor**: Solid block instead of line
- **Clipped**: Results stay within rounded container
- **Two selection styles**:
  - Normal: Accent color background, white text
  - Inverted (`--invert`): Black background, accent color text

## Building from Source

```bash
# Build release binary
cargo build --release

# Build and install to ~/.local/bin
just install

# Run tests
echo -e "a\nb\nc" | cargo run
```

## Dependencies

- Rust 1.70+
- Wayland compositor with wlr-layer-shell support (Sway, Niri, Hyprland, etc.)
- fontconfig (for `--font` matching)

## License

MIT
