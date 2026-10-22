//! Colours, palettes and widget styles
//!
//! Translucency only ever affects backgrounds. Text colours stay fully
//! opaque and cards keep a high alpha so text over a blurred wallpaper stays
//! readable

use iced::theme::{Mode as ThemeMode, Palette};
use iced::widget::{button, container};
use iced::{Background, Border, Color, Shadow, Theme};
use serde::{Deserialize, Serialize};

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Appearance {
    /// Translucent under Hyprland and other compositors known to handle it,
    /// opaque elsewhere; colours follow the system light or dark preference
    #[default]
    Auto,
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

pub fn palette(dark: bool) -> Palette {
    if dark {
        Palette {
            background: Color::from_rgb8(0x14, 0x17, 0x1c),
            text: Color::from_rgb8(0xe8, 0xec, 0xf1),
            primary: Color::from_rgb8(0x3b, 0x9e, 0xff),
            success: Color::from_rgb8(0x34, 0xc7, 0x7b),
            warning: Color::from_rgb8(0xf5, 0xa5, 0x24),
            danger: Color::from_rgb8(0xf0, 0x5d, 0x5e),
        }
    } else {
        Palette {
            background: Color::from_rgb8(0xf3, 0xf5, 0xf8),
            text: Color::from_rgb8(0x16, 0x1a, 0x21),
            primary: Color::from_rgb8(0x1f, 0x6f, 0xeb),
            success: Color::from_rgb8(0x1a, 0x9a, 0x55),
            warning: Color::from_rgb8(0xc2, 0x7a, 0x00),
            danger: Color::from_rgb8(0xd0, 0x3a, 0x3c),
        }
    }
}

pub fn theme(dark: bool) -> Theme {
    Theme::custom(
        if dark {
            "Dell Fan Dark"
        } else {
            "Dell Fan Light"
        },
        palette(dark),
    )
}

pub fn is_dark(mode: ThemeMode) -> bool {
    !matches!(mode, ThemeMode::Light)
}

/// Window background alpha for the chosen look
///
/// Compositor blur cannot be relied on (it can be disabled per window by
/// rules), so the values assume no blur at all: only the gaps between the
/// cards are translucent, every block of text sits on a nearly solid card
pub fn window_alpha(transparent_surface: bool, appearance: Appearance) -> f32 {
    if !transparent_surface {
        return 1.0;
    }
    match appearance {
        Appearance::Opaque => 1.0,
        _ => 0.90,
    }
}

/// Card background alpha, kept high for readability in every mode
pub fn card_alpha(window_alpha: f32) -> f32 {
    window_alpha.max(0.95)
}

pub fn muted(theme: &Theme) -> Color {
    let p = theme.palette();
    Color { a: 0.68, ..p.text }
}

pub fn card(theme: &Theme, alpha: f32) -> container::Style {
    let p = theme.extended_palette();
    let base = p.background.weak.color;
    container::Style {
        background: Some(Background::Color(Color {
            a: card_alpha(alpha),
            ..base
        })),
        border: Border {
            color: Color {
                a: 0.35,
                ..p.background.strong.color
            },
            width: 1.0,
            radius: 14.0.into(),
        },
        shadow: Shadow::default(),
        text_color: None,
        snap: true,
    }
}

pub fn banner(theme: &Theme, kind: Banner) -> container::Style {
    let p = theme.palette();
    let c = match kind {
        Banner::Info => p.primary,
        Banner::Ok => p.success,
        Banner::Warn => p.warning,
        Banner::Error => p.danger,
    };
    container::Style {
        background: Some(Background::Color(Color { a: 0.16, ..c })),
        border: Border {
            color: Color { a: 0.55, ..c },
            width: 1.0,
            radius: 10.0.into(),
        },
        shadow: Shadow::default(),
        text_color: None,
        snap: true,
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Banner {
    Info,
    Ok,
    Warn,
    Error,
}

pub fn segment(theme: &Theme, status: button::Status, selected: bool) -> button::Style {
    let p = theme.extended_palette();
    let (bg, fg) = if selected {
        (p.primary.base.color, p.primary.base.text)
    } else {
        let mut c = p.background.strong.color;
        c.a = match status {
            button::Status::Hovered => 0.55,
            button::Status::Pressed => 0.75,
            _ => 0.30,
        };
        (c, theme.palette().text)
    };
    button::Style {
        background: Some(Background::Color(bg)),
        text_color: if status == button::Status::Disabled {
            Color { a: 0.45, ..fg }
        } else {
            fg
        },
        border: Border {
            radius: 10.0.into(),
            ..Border::default()
        },
        shadow: Shadow::default(),
        snap: true,
    }
}

pub fn danger_button(theme: &Theme, status: button::Status) -> button::Style {
    let p = theme.palette();
    let a = match status {
        button::Status::Hovered => 0.95,
        button::Status::Pressed => 1.0,
        _ => 0.85,
    };
    button::Style {
        background: Some(Background::Color(Color { a, ..p.danger })),
        text_color: Color::WHITE,
        border: Border {
            radius: 10.0.into(),
            ..Border::default()
        },
        shadow: Shadow::default(),
        snap: true,
    }
}
