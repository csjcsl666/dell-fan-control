//! Dark glass look
//!
//! The visual language follows the glass panels of tr1xem/AWCC (dark blue
//! grey glass, thin light edges, accent outlines instead of solid fills, slim
//! sliders with a cyan accent) without copying its layout or assets.
//!
//! iced cannot blur what is behind the window; that is the compositor's job.
//! Hyprland only blurs a window when no rule disables it, and the
//! illogical-impulse rules on this machine disable blur for every window. The
//! glass is therefore built so it also holds without blur: a tinted gradient
//! base, layered panels with different opacity, edge highlights and shadows.
//! Text colours stay fully opaque in every mode

use iced::border::Radius;
use iced::theme::{Mode as ThemeMode, Palette};
use iced::widget::{button, container, slider, toggler};
use iced::{Background, Border, Color, Font, Gradient, Radians, Shadow, Theme, Vector, gradient};
use serde::{Deserialize, Serialize};
use std::sync::OnceLock;

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Appearance {
    /// Glass tuned to stay readable without compositor blur
    #[default]
    Auto,
    /// More see through; meant for setups where the compositor blurs the window
    Translucent,
    Opaque,
}

impl Appearance {
    pub const ALL: [Appearance; 3] = [
        Appearance::Auto,
        Appearance::Translucent,
        Appearance::Opaque,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Appearance::Auto => "自动",
            Appearance::Translucent => "半透明",
            Appearance::Opaque => "不透明",
        }
    }

    /// Whether the window should be created with a transparent surface
    pub fn wants_transparent_surface(self) -> bool {
        match self {
            Appearance::Auto => compositor_handles_transparency(),
            Appearance::Translucent => true,
            Appearance::Opaque => false,
        }
    }
}

/// Wayland compositors with reliable per window alpha. Unknown environments
/// fall back to the opaque look
pub fn compositor_handles_transparency() -> bool {
    let desktop = std::env::var("XDG_CURRENT_DESKTOP")
        .unwrap_or_default()
        .to_ascii_lowercase();
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
    wayland
        && (std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some()
            || ["hyprland", "niri", "kde", "sway", "wayfire"]
                .iter()
                .any(|d| desktop.contains(d)))
}

/// Opacity of each glass layer
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Glass {
    pub dark: bool,
    /// Window base
    pub base: f32,
    /// CPU and GPU cards
    pub card: f32,
    /// Function panels
    pub panel: f32,
}

impl Glass {
    pub fn new(dark: bool, transparent_surface: bool, appearance: Appearance) -> Self {
        let (base, card, panel) = if !transparent_surface || appearance == Appearance::Opaque {
            (1.0, 1.0, 1.0)
        } else if appearance == Appearance::Translucent {
            (0.62, 0.70, 0.66)
        } else {
            // without blur the base must hide background detail on its own
            (0.84, 0.86, 0.84)
        };
        Glass {
            dark,
            base,
            card,
            panel,
        }
    }
}

const ACCENT: Color = Color::from_rgb(0.133, 0.827, 0.933); // cyan, #22d3ee

pub fn accent() -> Color {
    ACCENT
}

fn rgb(r: u8, g: u8, b: u8) -> Color {
    Color::from_rgb8(r, g, b)
}

fn with_a(c: Color, a: f32) -> Color {
    Color { a, ..c }
}

fn mix(a: Color, b: Color, t: f32) -> Color {
    Color {
        r: a.r + (b.r - a.r) * t,
        g: a.g + (b.g - a.g) * t,
        b: a.b + (b.b - a.b) * t,
        a: a.a + (b.a - a.a) * t,
    }
}

pub fn palette(dark: bool) -> Palette {
    if dark {
        Palette {
            background: rgb(0x10, 0x17, 0x22),
            text: rgb(0xee, 0xf3, 0xf8),
            primary: ACCENT,
            success: rgb(0x3d, 0xd6, 0x8c),
            warning: rgb(0xf6, 0xb2, 0x3c),
            danger: rgb(0xf2, 0x67, 0x6a),
        }
    } else {
        Palette {
            background: rgb(0xe9, 0xee, 0xf4),
            text: rgb(0x14, 0x1b, 0x26),
            primary: rgb(0x08, 0x91, 0xb2),
            success: rgb(0x16, 0x8f, 0x55),
            warning: rgb(0xb4, 0x6f, 0x00),
            danger: rgb(0xc9, 0x3a, 0x3c),
        }
    }
}

pub fn theme(dark: bool) -> Theme {
    Theme::custom(
        if dark {
            "Dell Fan Glass Dark"
        } else {
            "Dell Fan Glass Light"
        },
        palette(dark),
    )
}

pub fn is_dark(mode: ThemeMode) -> bool {
    !matches!(mode, ThemeMode::Light)
}

/// Colour behind the root container; the root gradient does the real work
pub fn window_background(g: Glass) -> Color {
    with_a(
        palette(g.dark).background,
        if g.base >= 1.0 { 1.0 } else { 0.0 },
    )
}

/// Secondary text: lighter than the main text but never washed out grey
pub fn muted(theme: &Theme) -> Color {
    let p = theme.palette();
    if p.background.r < 0.5 {
        Color::from_rgba8(0xc6, 0xd3, 0xe2, 0.86)
    } else {
        Color::from_rgba8(0x3a, 0x48, 0x5a, 0.92)
    }
}

fn surface_tones(dark: bool) -> (Color, Color, Color) {
    if dark {
        // top of the window, bottom of the window, panel tint
        (
            rgb(0x1b, 0x27, 0x36),
            rgb(0x0b, 0x10, 0x18),
            rgb(0x17, 0x22, 0x33),
        )
    } else {
        (
            rgb(0xf4, 0xf7, 0xfb),
            rgb(0xdc, 0xe4, 0xee),
            rgb(0xf7, 0xfa, 0xfd),
        )
    }
}

fn linear(angle_deg: f32, stops: &[(f32, Color)]) -> Background {
    let mut l = gradient::Linear::new(Radians(angle_deg.to_radians()));
    for &(o, c) in stops {
        l = l.add_stop(o, c);
    }
    Background::Gradient(Gradient::Linear(l))
}

/// Layer 1: the whole window
pub fn root(g: Glass) -> container::Style {
    let (top, bottom, _) = surface_tones(g.dark);
    container::Style {
        background: Some(linear(
            160.0,
            &[
                (0.0, with_a(top, g.base)),
                (0.55, with_a(mix(top, bottom, 0.6), g.base)),
                (1.0, with_a(bottom, (g.base + 0.04).min(1.0))),
            ],
        )),
        border: Border::default(),
        shadow: Shadow::default(),
        text_color: None,
        snap: true,
    }
}

fn edge(dark: bool, a: f32) -> Color {
    if dark {
        Color::from_rgba(1.0, 1.0, 1.0, a)
    } else {
        Color::from_rgba(0.08, 0.12, 0.2, a * 0.9)
    }
}

/// Layer 2: CPU and GPU cards, a lighter sheet with a top highlight
pub fn card(g: Glass) -> container::Style {
    let (top, _, tint) = surface_tones(g.dark);
    let sheen = if g.dark {
        mix(tint, Color::WHITE, 0.07)
    } else {
        Color::WHITE
    };
    container::Style {
        background: Some(linear(
            180.0,
            &[
                (0.0, with_a(sheen, g.card)),
                (1.0, with_a(mix(tint, top, 0.3), g.card * 0.94)),
            ],
        )),
        border: Border {
            color: edge(g.dark, 0.11),
            width: 1.0,
            radius: 18.0.into(),
        },
        shadow: Shadow {
            color: Color::from_rgba(0.0, 0.0, 0.0, if g.dark { 0.38 } else { 0.12 }),
            offset: Vector::new(0.0, 8.0),
            blur_radius: 22.0,
        },
        text_color: None,
        snap: true,
    }
}

/// Layer 3: function panels, flatter and cooler than the cards
pub fn panel(g: Glass) -> container::Style {
    let (_, bottom, tint) = surface_tones(g.dark);
    let cool = if g.dark {
        mix(tint, rgb(0x1e, 0x3a, 0x5a), 0.25)
    } else {
        tint
    };
    container::Style {
        background: Some(linear(
            180.0,
            &[
                (0.0, with_a(cool, g.panel)),
                (1.0, with_a(mix(cool, bottom, 0.25), g.panel)),
            ],
        )),
        border: Border {
            color: edge(g.dark, 0.08),
            width: 1.0,
            radius: 16.0.into(),
        },
        shadow: Shadow {
            color: Color::from_rgba(0.0, 0.0, 0.0, if g.dark { 0.22 } else { 0.06 }),
            offset: Vector::new(0.0, 4.0),
            blur_radius: 14.0,
        },
        text_color: None,
        snap: true,
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Tone {
    Info,
    Ok,
    Warn,
    Error,
    Neutral,
}

pub fn tone_color(theme: &Theme, tone: Tone) -> Color {
    let p = theme.palette();
    match tone {
        Tone::Info => p.primary,
        Tone::Ok => p.success,
        Tone::Warn => p.warning,
        Tone::Error => p.danger,
        Tone::Neutral => muted(theme),
    }
}

/// Notification bar: a glass strip tinted by its meaning
pub fn notice(theme: &Theme, g: Glass, tone: Tone) -> container::Style {
    let c = tone_color(theme, tone);
    let (_, _, tint) = surface_tones(g.dark);
    container::Style {
        background: Some(linear(
            90.0,
            &[
                (0.0, with_a(mix(tint, c, 0.20), g.panel.max(0.82))),
                (1.0, with_a(mix(tint, c, 0.07), g.panel.max(0.80))),
            ],
        )),
        border: Border {
            color: with_a(c, 0.38),
            width: 1.0,
            radius: 12.0.into(),
        },
        shadow: Shadow::default(),
        text_color: None,
        snap: true,
    }
}

/// Small status pill
pub fn chip(theme: &Theme, tone: Tone) -> container::Style {
    let c = tone_color(theme, tone);
    container::Style {
        background: Some(Background::Color(with_a(c, 0.16))),
        border: Border {
            color: with_a(c, 0.55),
            width: 1.0,
            radius: 999.0.into(),
        },
        shadow: Shadow::default(),
        text_color: Some(if tone == Tone::Neutral {
            muted(theme)
        } else {
            c
        }),
        snap: true,
    }
}

/// Dot used in front of notifications
pub fn dot(theme: &Theme, tone: Tone) -> container::Style {
    let c = tone_color(theme, tone);
    container::Style {
        background: Some(Background::Color(c)),
        border: Border {
            radius: 999.0.into(),
            ..Border::default()
        },
        shadow: Shadow {
            color: with_a(c, 0.6),
            offset: Vector::ZERO,
            blur_radius: 6.0,
        },
        text_color: None,
        snap: true,
    }
}

/// Glass button; the selected state is an accent outline with a soft glow
pub fn glass_button(theme: &Theme, status: button::Status, selected: bool) -> button::Style {
    let dark = theme.palette().background.r < 0.5;
    let text = theme.palette().text;
    let acc = theme.palette().primary;
    let lift = match status {
        button::Status::Hovered => 0.05,
        button::Status::Pressed => 0.09,
        _ => 0.0,
    };
    let (bg, border, shadow) = if selected {
        (
            linear(
                180.0,
                &[
                    (0.0, with_a(acc, 0.22 + lift)),
                    (1.0, with_a(acc, 0.10 + lift)),
                ],
            ),
            Border {
                color: with_a(acc, 0.92),
                width: 1.5,
                radius: 12.0.into(),
            },
            Shadow {
                color: with_a(acc, 0.32),
                offset: Vector::ZERO,
                blur_radius: 14.0,
            },
        )
    } else {
        (
            Background::Color(edge(dark, 0.055 + lift)),
            Border {
                color: edge(dark, 0.12 + lift),
                width: 1.0,
                radius: 12.0.into(),
            },
            Shadow::default(),
        )
    };
    button::Style {
        background: Some(bg),
        text_color: if status == button::Status::Disabled {
            with_a(text, 0.45)
        } else {
            text
        },
        border,
        shadow,
        snap: true,
    }
}

/// Quiet glass button for secondary actions
pub fn ghost_button(theme: &Theme, status: button::Status) -> button::Style {
    glass_button(theme, status, false)
}

pub fn danger_button(theme: &Theme, status: button::Status) -> button::Style {
    let c = theme.palette().danger;
    let lift = match status {
        button::Status::Hovered => 0.08,
        button::Status::Pressed => 0.14,
        _ => 0.0,
    };
    button::Style {
        background: Some(linear(
            180.0,
            &[(0.0, with_a(c, 0.30 + lift)), (1.0, with_a(c, 0.18 + lift))],
        )),
        text_color: if status == button::Status::Disabled {
            with_a(theme.palette().text, 0.45)
        } else {
            theme.palette().text
        },
        border: Border {
            color: with_a(c, 0.85),
            width: 1.2,
            radius: 12.0.into(),
        },
        shadow: Shadow {
            color: with_a(c, 0.25),
            offset: Vector::ZERO,
            blur_radius: 10.0,
        },
        snap: true,
    }
}

pub fn glass_toggler(theme: &Theme, status: toggler::Status) -> toggler::Style {
    let dark = theme.palette().background.r < 0.5;
    let acc = theme.palette().primary;
    let (on, hover, disabled) = match status {
        toggler::Status::Active { is_toggled } => (is_toggled, false, false),
        toggler::Status::Hovered { is_toggled } => (is_toggled, true, false),
        toggler::Status::Disabled { is_toggled } => (is_toggled, false, true),
    };
    let lift = if hover { 0.06 } else { 0.0 };
    let alpha = if disabled { 0.45 } else { 1.0 };
    toggler::Style {
        background: if on {
            linear(
                90.0,
                &[
                    (0.0, with_a(acc, (0.55 + lift) * alpha)),
                    (1.0, with_a(acc, (0.80 + lift) * alpha)),
                ],
            )
        } else {
            Background::Color(edge(dark, (0.10 + lift) * alpha))
        },
        background_border_width: 1.0,
        background_border_color: if on {
            with_a(acc, 0.95 * alpha)
        } else {
            edge(dark, 0.22 * alpha)
        },
        foreground: Background::Color(Color::from_rgba(
            1.0,
            1.0,
            1.0,
            if on { 0.98 } else { 0.82 } * alpha,
        )),
        foreground_border_width: 0.0,
        foreground_border_color: Color::TRANSPARENT,
        text_color: None,
        border_radius: None,
        padding_ratio: 0.12,
    }
}

pub fn glass_slider(theme: &Theme, status: slider::Status) -> slider::Style {
    let dark = theme.palette().background.r < 0.5;
    let acc = theme.palette().primary;
    let r = match status {
        slider::Status::Active => 7.0,
        slider::Status::Hovered => 8.0,
        slider::Status::Dragged => 9.0,
    };
    slider::Style {
        rail: slider::Rail {
            backgrounds: (
                linear(90.0, &[(0.0, with_a(acc, 0.55)), (1.0, acc)]),
                Background::Color(edge(dark, 0.12)),
            ),
            width: 5.0,
            border: Border {
                radius: Radius::from(3.0),
                ..Border::default()
            },
        },
        handle: slider::Handle {
            shape: slider::HandleShape::Circle { radius: r },
            background: Background::Color(Color::WHITE),
            border_width: 2.0,
            border_color: acc,
        },
    }
}

static CJK_FAMILY: OnceLock<Option<&'static str>> = OnceLock::new();

pub fn set_family(f: Option<&'static str>) {
    let _ = CJK_FAMILY.set(f);
}

/// The chosen Chinese family at a given weight
pub fn font(weight: iced::font::Weight) -> Font {
    let base = match CJK_FAMILY.get().copied().flatten() {
        Some(f) => Font::with_name(f),
        None => Font::DEFAULT,
    };
    Font { weight, ..base }
}
