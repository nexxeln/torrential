use anstyle::{Color, RgbColor, Style};

pub const MUTED: RgbColor = RgbColor(0xA0, 0xA0, 0xA0);
pub const ACCENT: RgbColor = RgbColor(0xFF, 0xC7, 0x99);
pub const MINT: RgbColor = RgbColor(0x99, 0xFF, 0xE4);
pub const ERROR: RgbColor = RgbColor(0xFF, 0x80, 0x80);

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Token {
    #[default]
    Text,
    Muted,
    Accent,
    Mint,
    Error,
    Heading,
}

impl Token {
    fn rgb(self) -> Option<RgbColor> {
        match self {
            Self::Text => None,
            Self::Muted | Self::Heading => Some(MUTED),
            Self::Accent => Some(ACCENT),
            Self::Mint => Some(MINT),
            Self::Error => Some(ERROR),
        }
    }

    pub fn style(self) -> Style {
        let style = Style::new().fg_color(self.rgb().map(Color::Rgb));
        if self == Self::Heading { style.bold() } else { style }
    }

    /// For progress bar templates, which take colors as `#rrggbb`.
    pub fn bar_color(self) -> String {
        self.rgb().map_or_else(|| "white".to_string(), |RgbColor(r, g, b)| format!("#{r:02x}{g:02x}{b:02x}"))
    }
}

pub fn paint(token: Token, text: &str) -> String {
    let style = token.style();
    if style == Style::new() { text.to_string() } else { format!("{style}{text}{style:#}") }
}
