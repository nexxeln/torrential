use std::fmt;
use std::ops::RangeInclusive;

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize)]
pub struct Quality {
    /// `None` when the name carries no resolution tag.
    pub resolution: Option<Resolution>,
    pub codec: Option<Codec>,
    /// HDR10, HDR10+, or Dolby Vision. False when the name does not say, which
    /// usually means SDR.
    pub hdr: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Resolution {
    /// 576 lines or fewer, such as a DVD rip.
    #[serde(rename = "sd")]
    Sd,
    #[serde(rename = "720p")]
    P720,
    #[serde(rename = "1080p")]
    P1080,
    #[serde(rename = "2160p")]
    P2160,
}

impl Resolution {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Sd => "sd",
            Self::P720 => "720p",
            Self::P1080 => "1080p",
            Self::P2160 => "2160p",
        }
    }

    fn from_dimensions(token: &str) -> Option<Resolution> {
        let (width, height) = token.split_once('x')?;
        if width.len() < 3 || height.len() < 3 || !height.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        Some(match width.parse::<u32>().ok()? {
            3800.. => Self::P2160,
            1900.. => Self::P1080,
            1200.. => Self::P720,
            _ => Self::Sd,
        })
    }
}

/// The Mbit/s at each resolution that looks right and streams comfortably. A small encode of a
/// popular release sits low in it, and a remux runs above it.
pub fn band(resolution: Option<Resolution>) -> RangeInclusive<f64> {
    match resolution {
        Some(Resolution::Sd) => 0.3..=4.0,
        Some(Resolution::P720) => 0.6..=8.0,
        Some(Resolution::P1080) => 1.0..=16.0,
        Some(Resolution::P2160) => 3.0..=40.0,
        None => 0.3..=16.0,
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Codec {
    /// x264, H.264, or AVC.
    H264,
    /// x265, H.265, or HEVC.
    Hevc,
    Av1,
}

impl Codec {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::H264 => "h264",
            Self::Hevc => "hevc",
            Self::Av1 => "av1",
        }
    }
}

impl Quality {
    /// The first resolution and codec tags in the name. Tags are whole words, so
    /// `x264aac` and a bare `HD` stay unknown.
    pub fn parse(name: &str) -> Quality {
        let tokens: Vec<String> = name
            .split(|c: char| !c.is_ascii_alphanumeric())
            .filter(|t| !t.is_empty())
            .map(str::to_ascii_lowercase)
            .collect();
        let mut quality = Quality::default();
        let mut dolby_vision = false;
        for (i, token) in tokens.iter().enumerate() {
            let pair = (token.as_str(), tokens.get(i + 1).map_or("", String::as_str));
            let resolution = match pair {
                ("2160p" | "4k" | "uhd", _) | ("ultra", "hd") => Some(Resolution::P2160),
                ("1080p" | "1080i" | "fhd", _) | ("full", "hd") => Some(Resolution::P1080),
                ("720p", _) => Some(Resolution::P720),
                ("576p" | "576i" | "480p" | "480i" | "360p" | "dvdrip", _) => Some(Resolution::Sd),
                (token, _) => Resolution::from_dimensions(token),
            };
            let codec = match pair {
                ("x265" | "h265" | "hevc", _) | ("h", "265") => Some(Codec::Hevc),
                ("x264" | "h264" | "avc", _) | ("h", "264") => Some(Codec::H264),
                ("av1", _) => Some(Codec::Av1),
                _ => None,
            };
            quality.resolution = quality.resolution.or(resolution);
            quality.codec = quality.codec.or(codec);
            quality.hdr |= matches!(pair, ("hdr" | "hdr10" | "hdr10plus" | "dovi", _) | ("dolby", "vision"));
            dolby_vision |= pair.0 == "dv";
        }
        quality.hdr |=
            dolby_vision && (quality.resolution == Some(Resolution::P2160) || quality.codec == Some(Codec::Hevc));
        quality
    }
}

/// `2160p hevc hdr`, `1080p h264`, or nothing when the name says nothing.
impl fmt::Display for Quality {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let hdr = self.hdr.then_some("hdr");
        let parts = [self.resolution.map(Resolution::as_str), self.codec.map(Codec::as_str), hdr];
        f.write_str(&parts.into_iter().flatten().collect::<Vec<_>>().join(" "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_tags_from_recorded_names() {
        let cases = [
            ("Celtics City S01E08 Chapter VIII Ubuntu 2160p MAX WEB-DL DDP5 1 H 265-RAWR", "2160p hevc"),
            ("Celtics City S01E08 Chapter VIII Ubuntu 1080p AMZN WEB-DL DDP5 1 H 264-Kits", "1080p h264"),
            ("Celtics City S01E08 Chapter VIII Ubuntu 720p AMZN WEB-DL DDP5 1 H 264-RAWR", "720p h264"),
            ("Aerial Africa Series 1 2of6 Spirit of Ubuntu 1080p x264 AAC HDTV", "1080p h264"),
            ("5 Centimeters Per Second [BD][Web2.0][blogs][linux][bsd][eng][x264aac]", ""),
            ("Ubuntu 11.04 Desktop dvd amd64 (x64) .iso", ""),
            ("ubuntu-24.04.1-desktop-amd64.iso", ""),
            ("Big Buck Bunny (2008) 4K 60FPS Blender Animated Short - x265", "2160p hevc"),
            ("Big Buck Bunny (AV1 Ripped by Martin Eesmaa + 7z archives compressed)", "av1"),
            ("Test Video Contents for Encoders - Big Buck Bunny H265/HEVC (Lossless)", "hevc"),
            ("big buck bunny 720p h264", "720p h264"),
            ("big_buck_bunny_1080p_CLHD", "1080p"),
            ("Night of the Living Dead (1968) 4K UHD Blu-ray Remux", "2160p"),
            ("Night of the Living Dead (1968) [colorised, 4k, 60FPS]", "2160p"),
            ("Night Of The Living Dead ( 1968) English FULL HD", "1080p"),
            ("Night of the Living Dead |1968| WebRip US (HD 1080p)", "1080p"),
            ("Night Of The Living Dead 720p 1968", "720p"),
            ("Night Of The Living Dead (1968) [HD]", ""),
            ("NIGHT OF THE LIVING DEAD (1968) HDTV UK", ""),
            ("Night of the Living Dead (1968)", ""),
        ];
        let read: Vec<(&str, String)> =
            cases.iter().map(|(name, _)| (*name, Quality::parse(name).to_string())).collect();
        assert_eq!(read, cases.map(|(name, quality)| (name, quality.to_string())));
    }

    #[test]
    fn reads_hdr_dimensions_and_sd_forms() {
        let cases = [
            ("Movie 2024 2160p WEB-DL DV HDR10+ HEVC-GRP", "2160p hevc hdr"),
            ("Movie 2024 2160p BluRay Dolby Vision x265", "2160p hevc hdr"),
            ("Movie.2024.HDR.2160p.WEB.h265", "2160p hevc hdr"),
            ("[Group] Show - 01 [1920x1080 AVC]", "1080p h264"),
            ("[Group] Show - 01 (3840x1600 HEVC)", "2160p hevc"),
            ("Show S01E02 1x02 576p DVDRip XviD", "sd"),
            ("Family Holiday 1998 DV tape transfer", ""),
            ("Movie 2024 1080p HDRip x264", "1080p h264"),
        ];
        let read: Vec<(&str, String)> =
            cases.iter().map(|(name, _)| (*name, Quality::parse(name).to_string())).collect();
        assert_eq!(read, cases.map(|(name, quality)| (name, quality.to_string())));
    }
}
