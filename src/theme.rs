//! Colours and text styles: Catppuccin accents on the terminal's own
//! background (Mocha on dark terminals, Latte on light ones), 24-bit or
//! 256-colour as the terminal allows. With `NO_COLOR` set, only bold /
//! italic / underline / dim / reverse are used.

use ratatui::style::{Color, Modifier, Style};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    pub color: bool,
    /// The terminal has a light background: use the light palette.
    pub light: bool,
    /// The terminal takes 24-bit colour; otherwise the nearest of the 256
    /// indexed colours is used.
    pub truecolor: bool,
}

type Rgb = (u8, u8, u8);

/// One Catppuccin flavour.
struct Palette {
    red: Rgb,
    peach: Rgb,
    yellow: Rgb,
    green: Rgb,
    teal: Rgb,
    sapphire: Rgb,
    blue: Rgb,
    lavender: Rgb,
    mauve: Rgb,
    subtext0: Rgb,
    overlay1: Rgb,
    overlay0: Rgb,
    surface1: Rgb,
    surface0: Rgb,
    text: Rgb,
    base: Rgb,
    code_bg: Rgb,
    match_bg: Rgb,
}

/// Catppuccin Mocha, for dark terminals.
const MOCHA: Palette = Palette {
    red: (0xf3, 0x8b, 0xa8),
    peach: (0xfa, 0xb3, 0x87),
    yellow: (0xf9, 0xe2, 0xaf),
    green: (0xa6, 0xe3, 0xa1),
    teal: (0x94, 0xe2, 0xd5),
    sapphire: (0x74, 0xc7, 0xec),
    blue: (0x89, 0xb4, 0xfa),
    lavender: (0xb4, 0xbe, 0xfe),
    mauve: (0xcb, 0xa6, 0xf7),
    subtext0: (0xa6, 0xad, 0xc8),
    overlay1: (0x7f, 0x84, 0x9c),
    overlay0: (0x6c, 0x70, 0x86),
    surface1: (0x45, 0x47, 0x5a),
    surface0: (0x31, 0x32, 0x44),
    text: (0xcd, 0xd6, 0xf4),
    base: (0x1e, 0x1e, 0x2e),
    code_bg: (0x26, 0x27, 0x37),
    match_bg: (0x5b, 0x51, 0x3a),
};

/// Catppuccin Latte, for light terminals.
const LATTE: Palette = Palette {
    red: (0xd2, 0x0f, 0x39),
    peach: (0xfe, 0x64, 0x0b),
    yellow: (0xdf, 0x8e, 0x1d),
    green: (0x40, 0xa0, 0x2b),
    teal: (0x17, 0x92, 0x99),
    sapphire: (0x20, 0x9f, 0xb5),
    blue: (0x1e, 0x66, 0xf5),
    lavender: (0x72, 0x87, 0xfd),
    mauve: (0x88, 0x39, 0xef),
    subtext0: (0x6c, 0x6f, 0x85),
    overlay1: (0x8c, 0x8f, 0xa1),
    overlay0: (0x9c, 0xa0, 0xb0),
    surface1: (0xbc, 0xc0, 0xcc),
    surface0: (0xcc, 0xd0, 0xda),
    text: (0x4c, 0x4f, 0x69),
    base: (0xef, 0xf1, 0xf5),
    code_bg: (0xe6, 0xe9, 0xef),
    match_bg: (0xf2, 0xdf, 0xb4),
};

impl Theme {
    /// Dark palette, 24-bit colour (or no colour at all).
    pub const fn new(color: bool) -> Theme {
        Theme {
            color,
            light: false,
            truecolor: true,
        }
    }

    /// From the environment: no colours when `NO_COLOR` is set to a
    /// non-empty value; the light palette when `LUMBRIDGE_NOTES_THEME=light`
    /// (or the same variable under an old prefix) or `COLORFGBG` reports a light
    /// background; 24-bit colour when `COLORTERM` says so, else the
    /// 256-colour palette.
    pub fn from_env() -> Theme {
        let var = |name: &str| std::env::var(name).ok();
        Theme::from_vars(
            var("NO_COLOR").as_deref(),
            crate::naming::env_var("THEME")
                .and_then(|theme| theme.into_string().ok())
                .as_deref(),
            var("COLORFGBG").as_deref(),
            var("COLORTERM").as_deref(),
        )
    }

    fn from_vars(
        no_color: Option<&str>,
        theme: Option<&str>,
        colorfgbg: Option<&str>,
        colorterm: Option<&str>,
    ) -> Theme {
        let light = match theme.map(str::trim) {
            Some("light") => true,
            Some("dark") => false,
            _ => colorfgbg.is_some_and(light_background),
        };
        Theme {
            color: no_color.is_none_or(str::is_empty),
            light,
            truecolor: colorterm.is_some_and(|value| {
                let value = value.to_ascii_lowercase();
                value.contains("truecolor") || value.contains("24bit")
            }),
        }
    }

    fn palette(self) -> &'static Palette {
        if self.light {
            &LATTE
        } else {
            &MOCHA
        }
    }

    fn rgb(self, (r, g, b): Rgb) -> Color {
        if self.truecolor {
            Color::Rgb(r, g, b)
        } else {
            Color::Indexed(nearest_indexed(r, g, b))
        }
    }

    fn fg(self, color: Rgb) -> Style {
        if self.color {
            Style::default().fg(self.rgb(color))
        } else {
            Style::default()
        }
    }

    fn bg(self, style: Style, color: Rgb) -> Style {
        if self.color {
            style.bg(self.rgb(color))
        } else {
            style
        }
    }

    /// A tinted background with the palette's own text colour on it, so it
    /// stays readable whatever the terminal's default foreground is.
    fn panel(self, color: Rgb) -> Style {
        let text = self.palette().text;
        self.bg(self.fg(text), color)
    }

    /// Secondary text (paths, hints, front matter, quotes).
    pub fn dim(self) -> Style {
        if self.color {
            Style::default().fg(self.rgb(self.palette().overlay1))
        } else {
            Style::default().add_modifier(Modifier::DIM)
        }
    }

    /// Markdown syntax shown on raw lines.
    pub fn marker(self) -> Style {
        if self.color {
            Style::default().fg(self.rgb(self.palette().overlay0))
        } else {
            Style::default().add_modifier(Modifier::DIM)
        }
    }

    pub fn heading(self, level: u8) -> Style {
        let color = match level {
            1 => self.palette().red,
            2 => self.palette().peach,
            3 => self.palette().yellow,
            4 => self.palette().green,
            5 => self.palette().sapphire,
            _ => self.palette().lavender,
        };
        self.fg(color).add_modifier(Modifier::BOLD)
    }

    pub fn link(self) -> Style {
        self.fg(self.palette().blue)
            .add_modifier(Modifier::UNDERLINED)
    }

    pub fn inline_code(self) -> Style {
        if self.color {
            self.bg(self.fg(self.palette().peach), self.palette().surface0)
        } else {
            // The backticks are hidden: keep code distinct from prose.
            Style::default().add_modifier(Modifier::REVERSED)
        }
    }

    pub fn code_block(self) -> Style {
        if self.color {
            self.panel(self.palette().code_bg)
        } else {
            Style::default().add_modifier(Modifier::DIM)
        }
    }

    pub fn code_label(self) -> Style {
        self.dim().add_modifier(Modifier::ITALIC)
    }

    pub fn bullet(self) -> Style {
        self.fg(self.palette().mauve)
    }

    pub fn number(self) -> Style {
        self.fg(self.palette().mauve)
    }

    pub fn checkbox(self, checked: bool) -> Style {
        if checked {
            self.fg(self.palette().green)
        } else {
            self.fg(self.palette().blue)
        }
    }

    pub fn quote_bar(self) -> Style {
        self.fg(self.palette().lavender)
    }

    pub fn quote_text(self) -> Style {
        if self.color {
            Style::default()
                .fg(self.rgb(self.palette().subtext0))
                .add_modifier(Modifier::ITALIC)
        } else {
            Style::default().add_modifier(Modifier::ITALIC)
        }
    }

    pub fn rule(self) -> Style {
        if self.color {
            Style::default().fg(self.rgb(self.palette().surface1))
        } else {
            Style::default().add_modifier(Modifier::DIM)
        }
    }

    pub fn table_border(self) -> Style {
        self.marker()
    }

    pub fn html(self) -> Style {
        self.fg(self.palette().teal)
    }

    pub fn image(self) -> Style {
        self.fg(self.palette().mauve).add_modifier(Modifier::ITALIC)
    }

    pub fn front_matter(self) -> Style {
        self.dim().add_modifier(Modifier::ITALIC)
    }

    pub fn selection(self) -> Style {
        if self.color {
            self.panel(self.palette().surface1)
        } else {
            Style::default().add_modifier(Modifier::REVERSED)
        }
    }

    pub fn find_match(self) -> Style {
        if self.color {
            self.panel(self.palette().match_bg)
        } else {
            Style::default().add_modifier(Modifier::UNDERLINED)
        }
    }

    pub fn find_current(self) -> Style {
        if self.color {
            self.bg(self.fg(self.palette().base), self.palette().peach)
        } else {
            Style::default().add_modifier(Modifier::REVERSED)
        }
    }

    pub fn title(self) -> Style {
        self.fg(self.palette().lavender)
            .add_modifier(Modifier::BOLD)
    }

    pub fn unsaved(self) -> Style {
        self.fg(self.palette().peach)
    }

    pub fn accent(self) -> Style {
        self.fg(self.palette().mauve)
    }

    pub fn ok(self) -> Style {
        self.fg(self.palette().green)
    }

    pub fn warn(self) -> Style {
        self.fg(self.palette().yellow).add_modifier(Modifier::BOLD)
    }

    pub fn error(self) -> Style {
        self.fg(self.palette().red).add_modifier(Modifier::BOLD)
    }

    /// The highlighted row in the picker and search results.
    pub fn picked(self) -> Style {
        if self.color {
            self.panel(self.palette().surface0)
        } else {
            Style::default().add_modifier(Modifier::REVERSED)
        }
    }

    /// The matched text inside a search snippet.
    pub fn hit(self) -> Style {
        self.fg(self.palette().yellow).add_modifier(Modifier::BOLD)
    }

    /// A toolbar or panel button.
    pub fn button(self) -> Style {
        if self.color {
            self.panel(self.palette().surface0)
        } else {
            Style::default().add_modifier(Modifier::REVERSED)
        }
    }

    /// A button under the mouse.
    pub fn button_hover(self) -> Style {
        if self.color {
            self.panel(self.palette().surface1)
        } else {
            Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
        }
    }

    /// The button Enter presses, or a menu's highlighted entry.
    pub fn button_default(self) -> Style {
        if self.color {
            self.bg(self.fg(self.palette().base), self.palette().mauve)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
        }
    }

    /// A button whose job is running (recording, reading, naming).
    pub fn button_active(self) -> Style {
        if self.color {
            self.bg(self.fg(self.palette().base), self.palette().red)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
        }
    }

    /// A button for a service that is not set up.
    pub fn button_off(self) -> Style {
        if self.color {
            self.bg(self.fg(self.palette().overlay0), self.palette().surface0)
        } else {
            Style::default().add_modifier(Modifier::DIM)
        }
    }

    /// Background of a dropdown or the AI panel.
    pub fn popup(self) -> Style {
        if self.color {
            self.panel(self.palette().code_bg)
        } else {
            Style::default()
        }
    }

    /// The paragraph being read aloud (a bar in the margin).
    pub fn reading(self) -> Style {
        self.fg(self.palette().green).add_modifier(Modifier::BOLD)
    }

    /// A separator line between panes.
    pub fn border(self) -> Style {
        self.rule()
    }
}

/// Whether a `COLORFGBG` value (`fg;bg`, sometimes `fg;default;bg`) names a
/// light background colour (7 or 9-15 in the 16-colour palette).
fn light_background(value: &str) -> bool {
    value
        .rsplit(';')
        .next()
        .and_then(|bg| bg.trim().parse::<u8>().ok())
        .is_some_and(|bg| bg == 7 || (9..=15).contains(&bg))
}

/// The xterm 256-colour index closest to an RGB colour (from the 6x6x6
/// cube and the grey ramp).
fn nearest_indexed(r: u8, g: u8, b: u8) -> u8 {
    const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    let level = |value: u8| -> usize {
        LEVELS
            .iter()
            .enumerate()
            .min_by_key(|(_, level)| (i32::from(**level) - i32::from(value)).abs())
            .map_or(0, |(index, _)| index)
    };
    let distance = |(r2, g2, b2): (u8, u8, u8)| -> i32 {
        let d = |a: u8, b: u8| (i32::from(a) - i32::from(b)).pow(2);
        d(r, r2) + d(g, g2) + d(b, b2)
    };
    let (ri, gi, bi) = (level(r), level(g), level(b));
    let cube = (LEVELS[ri], LEVELS[gi], LEVELS[bi]);
    let cube_index = 16 + 36 * ri + 6 * gi + bi;
    let average = (u32::from(r) + u32::from(g) + u32::from(b)) / 3;
    let grey_step = (average.saturating_sub(8) / 10).min(23) as u8;
    let grey_value = 8 + 10 * grey_step;
    let grey = (grey_value, grey_value, grey_value);
    if distance(grey) < distance(cube) {
        232 + grey_step
    } else {
        cube_index as u8
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_environment() {
        let theme = Theme::from_vars(None, None, None, Some("truecolor"));
        assert_eq!(theme, Theme::new(true));
        assert!(!Theme::from_vars(Some("1"), None, None, None).color);
        assert!(Theme::from_vars(Some(""), None, None, None).color);
        assert!(Theme::from_vars(None, None, Some("0;15"), None).light);
        assert!(Theme::from_vars(None, None, Some("0;default;7"), None).light);
        assert!(!Theme::from_vars(None, None, Some("15;0"), None).light);
        assert!(!Theme::from_vars(None, Some("dark"), Some("0;15"), None).light);
        assert!(Theme::from_vars(None, Some("light"), None, None).light);
        assert!(!Theme::from_vars(None, None, None, None).truecolor);
    }

    #[test]
    fn backgrounds_carry_their_own_text_colour() {
        for light in [false, true] {
            let theme = Theme {
                light,
                ..Theme::new(true)
            };
            for style in [
                theme.code_block(),
                theme.selection(),
                theme.picked(),
                theme.find_match(),
                theme.find_current(),
                theme.inline_code(),
            ] {
                assert!(style.bg.is_some() && style.fg.is_some(), "{style:?}");
            }
        }
    }

    #[test]
    fn without_truecolor_colours_are_indexed() {
        let theme = Theme {
            truecolor: false,
            ..Theme::new(true)
        };
        assert!(matches!(theme.heading(1).fg, Some(Color::Indexed(_))));
        assert_eq!(nearest_indexed(0, 0, 0), 16);
        assert_eq!(nearest_indexed(255, 255, 255), 231);
        assert_eq!(nearest_indexed(0x80, 0x80, 0x80), 244);
        assert_eq!(nearest_indexed(255, 0, 0), 196);
    }

    #[test]
    fn no_colour_code_is_still_marked() {
        let theme = Theme::new(false);
        assert!(theme
            .inline_code()
            .add_modifier
            .contains(Modifier::REVERSED));
        assert!(theme.code_block().add_modifier.contains(Modifier::DIM));
    }
}
