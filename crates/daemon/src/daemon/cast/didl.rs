//! What a TV is told about the video it plays: the DIDL-Lite metadata that goes with
//! SetAVTransportURI, and the DLNA headers the file is served with.

use super::upnp::clock;
use super::xml::escape;

/// The fourth field of the video's protocolInfo and its `contentFeatures.dlna.org` header: seeks
/// by byte range (`OP=01`), served as it is (`CI=0`), streamed, with DLNA 1.5 flags.
pub const FEATURES: &str = "DLNA.ORG_OP=01;DLNA.ORG_CI=0;DLNA.ORG_FLAGS=01700000000000000000000000000000";

/// The subtitle file sent with a video.
pub struct Caption<'a> {
    pub url: &'a str,
    /// Its extension, such as `srt`, which Samsung TVs read as its type.
    pub kind: &'a str,
}

/// The video a TV is asked to play.
pub struct Item<'a> {
    pub title: &'a str,
    pub url: &'a str,
    /// The MIME type in the TV's own spelling.
    pub mime: &'a str,
    pub size: u64,
    /// Seconds, when known.
    pub duration: Option<f64>,
    pub film: bool,
    pub caption: Option<Caption<'a>>,
}

/// The DIDL-Lite document for `item`. Samsung TVs read the subtitle from `sec:CaptionInfoEx`, and
/// LG, Sony, and others from a second `res` whose type is the subtitle's.
pub fn didl(item: &Item) -> String {
    let class = if item.film { "object.item.videoItem.movie" } else { "object.item.videoItem" };
    let duration = item.duration.map_or(String::new(), |d| format!(" duration=\"{}.000\"", clock(d)));
    let mut res = format!(
        "<res protocolInfo=\"http-get:*:{}:{FEATURES}\" size=\"{}\"{duration}>{}</res>",
        escape(item.mime),
        item.size,
        escape(item.url)
    );
    if let Some(caption) = &item.caption {
        let (url, kind) = (escape(caption.url), escape(caption.kind));
        res.push_str(&format!(
            "<res protocolInfo=\"http-get:*:text/{kind}:*\">{url}</res>\
             <sec:CaptionInfoEx sec:type=\"{kind}\">{url}</sec:CaptionInfoEx>\
             <sec:CaptionInfo sec:type=\"{kind}\">{url}</sec:CaptionInfo>"
        ));
    }
    format!(
        "<DIDL-Lite xmlns=\"urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/\" \
         xmlns:dc=\"http://purl.org/dc/elements/1.1/\" xmlns:upnp=\"urn:schemas-upnp-org:metadata-1-0/upnp/\" \
         xmlns:dlna=\"urn:schemas-dlna-org:metadata-1-0/\" xmlns:sec=\"http://www.sec.co.kr/\">\
         <item id=\"0\" parentID=\"-1\" restricted=\"1\"><dc:title>{}</dc:title>\
         <upnp:class>{class}</upnp:class>{res}</item></DIDL-Lite>",
        escape(item.title)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_metadata_names_the_video_its_type_and_its_subtitle_the_ways_tvs_read_them() {
        let item = Item {
            title: "Night of the Living Dead (1968)",
            url: "http://192.168.1.23:47919/cast/5f0c/Night.mkv",
            mime: "video/x-mkv",
            size: 8_375_186_227,
            duration: Some(5_760.0),
            film: true,
            caption: Some(Caption { url: "http://192.168.1.23:47919/cast/9e1d/Night.srt", kind: "srt" }),
        };
        assert_eq!(
            didl(&item),
            "<DIDL-Lite xmlns=\"urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/\" \
             xmlns:dc=\"http://purl.org/dc/elements/1.1/\" xmlns:upnp=\"urn:schemas-upnp-org:metadata-1-0/upnp/\" \
             xmlns:dlna=\"urn:schemas-dlna-org:metadata-1-0/\" xmlns:sec=\"http://www.sec.co.kr/\">\
             <item id=\"0\" parentID=\"-1\" restricted=\"1\"><dc:title>Night of the Living Dead (1968)</dc:title>\
             <upnp:class>object.item.videoItem.movie</upnp:class>\
             <res protocolInfo=\"http-get:*:video/x-mkv:DLNA.ORG_OP=01;DLNA.ORG_CI=0;DLNA.ORG_FLAGS=01700000000000000000000000000000\" \
             size=\"8375186227\" duration=\"1:36:00.000\">http://192.168.1.23:47919/cast/5f0c/Night.mkv</res>\
             <res protocolInfo=\"http-get:*:text/srt:*\">http://192.168.1.23:47919/cast/9e1d/Night.srt</res>\
             <sec:CaptionInfoEx sec:type=\"srt\">http://192.168.1.23:47919/cast/9e1d/Night.srt</sec:CaptionInfoEx>\
             <sec:CaptionInfo sec:type=\"srt\">http://192.168.1.23:47919/cast/9e1d/Night.srt</sec:CaptionInfo>\
             </item></DIDL-Lite>"
        );
    }

    #[test]
    fn a_title_with_markup_in_it_stays_text() {
        let item = Item {
            title: "Tom & Jerry <Uncut>",
            url: "http://192.168.1.23:47919/cast/5f0c/a.mp4",
            mime: "video/mp4",
            size: 1,
            duration: None,
            film: false,
            caption: None,
        };
        let text = didl(&item);
        assert!(text.contains("<dc:title>Tom &amp; Jerry &lt;Uncut&gt;</dc:title>"), "{text}");
        assert!(text.contains("<upnp:class>object.item.videoItem</upnp:class>"));
        assert!(text.contains("size=\"1\">http"), "no duration when it is not known: {text}");
    }
}
