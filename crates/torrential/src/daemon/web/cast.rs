//! Play on TV for the web app: the TVs on the home network, starting a cast, and the remote
//! control. A paired phone may use all of it.

use serde::Deserialize;
use serde_json::{Value, json};
use torrential_core::{CastAction, CastRequest, Error, ErrorCode, Target, VideoRef};

use super::super::server::Ctx;

#[derive(Deserialize)]
pub struct Search {
    #[serde(default)]
    fresh: bool,
}

/// `POST /api/tvs`: the TVs and the casts, after a search of the home network unless one ran in
/// the last 30 seconds.
pub async fn search(ctx: &Ctx, request: Search) -> Result<Value, Error> {
    super::value(ctx.casts.search(request.fresh).await)
}

#[derive(Deserialize)]
pub struct Start {
    /// A TV's ID. Without it, the only TV on the network.
    tv: Option<String>,
    /// A film or an episode to play, or with `ref`, the one the torrent holds.
    video: Option<VideoRef>,
    /// For a film or an episode, the one release to try.
    source: Option<String>,
    /// A download or search result ID, an infohash, a magnet link, or the URL of a .torrent file.
    #[serde(rename = "ref")]
    reference: Option<String>,
    file: Option<u32>,
    subtitle: Option<u32>,
    start_at: Option<f64>,
}

/// `POST /api/cast`: starts a cast and answers with it at once. `GET /api/tvs` follows it.
pub async fn start(ctx: &Ctx, request: Start) -> Result<Value, Error> {
    let Start { tv, video, source, reference, file, subtitle, start_at } = request;
    let (target, video) = match (reference, video) {
        (Some(reference), video) => (super::target(&reference)?, video),
        (None, Some(video)) => (Target::Title(video), None),
        (None, None) => return Err(Error::new(ErrorCode::Usage, "the cast names nothing to play: give video or ref")),
    };
    let request = CastRequest { tv, target, source, file, video, subtitle, start_at };
    let (view, _) = ctx.casts.start(request).await?;
    let cast = view.borrow().clone();
    Ok(json!({ "cast": cast }))
}

#[derive(Deserialize)]
pub struct Control {
    /// A TV's ID. Without it, the only cast playing.
    tv: Option<String>,
    #[serde(flatten)]
    action: CastAction,
}

/// `POST /api/cast/control`: pauses, resumes, seeks, stops, or sets the volume of a cast.
pub async fn control(ctx: &Ctx, request: Control) -> Result<Value, Error> {
    let cast = ctx.casts.control(request.tv.as_deref(), request.action).await?;
    Ok(json!({ "cast": cast }))
}
