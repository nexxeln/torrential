//! The `preferences` block of `config.yaml`, which `torrential setup` writes. Each field
//! overrides what torrential detects about the machine, for someone who watches on another
//! screen or player. An absent field leaves the detected value. `screen` and `playback` rank
//! only for this machine: a browser plays what it decodes, and another device's browser
//! ranks for its own screen.

use serde::{Deserialize, Serialize};

use crate::quality::{Codec, Quality, Resolution};

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct Preferences {
    #[serde(default)]
    pub screen: Option<Screen>,
    /// Bytes.
    #[serde(default)]
    pub max_size: Option<u64>,
    #[serde(default)]
    pub playback: Option<Playback>,
}

impl Preferences {
    pub fn is_empty(&self) -> bool {
        *self == Preferences::default()
    }
}

/// Where the user watches, when that is not this machine's own display.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Screen {
    #[serde(rename = "phone")]
    Phone,
    #[serde(rename = "laptop")]
    Laptop,
    #[serde(rename = "tv")]
    Tv,
    #[serde(rename = "tv-4k")]
    Tv4k,
}

impl Screen {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Phone => "phone",
            Self::Laptop => "laptop",
            Self::Tv => "tv",
            Self::Tv4k => "tv-4k",
        }
    }

    /// The resolution that suits this screen. A phone shows 720p as well as 1080p, in a
    /// smaller file.
    pub const fn target(self) -> Resolution {
        match self {
            Self::Phone => Resolution::P720,
            Self::Laptop | Self::Tv => Resolution::P1080,
            Self::Tv4k => Resolution::P2160,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Playback {
    #[serde(rename = "hevc-hdr")]
    HevcHdr,
    #[serde(rename = "hevc")]
    Hevc,
    /// For a player that may not handle HEVC, since every player handles H.264.
    #[serde(rename = "h264")]
    H264,
}

impl Playback {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::HevcHdr => "hevc-hdr",
            Self::Hevc => "hevc",
            Self::H264 => "h264",
        }
    }

    /// AV1 is newer than HEVC, so a player that may not handle HEVC may not handle it
    /// either. An unknown codec plays.
    pub const fn plays(self, quality: Quality) -> bool {
        let newer_codec = matches!(quality.codec, Some(Codec::Hevc | Codec::Av1));
        match self {
            Self::HevcHdr => true,
            Self::Hevc => !quality.hdr,
            Self::H264 => !quality.hdr && !newer_codec,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_player_that_may_not_handle_hevc_or_hdr_plays_only_what_it_can() {
        let names = ["Film 1080p x264", "Film 2160p x265", "Film 2160p HDR x265", "Film 1080p AV1", "Film"];
        let plays = |playback: Playback| names.map(|name| playback.plays(Quality::parse(name)));
        assert_eq!(plays(Playback::HevcHdr), [true, true, true, true, true]);
        assert_eq!(plays(Playback::Hevc), [true, true, false, true, true]);
        assert_eq!(plays(Playback::H264), [true, false, false, false, true]);
    }
}
