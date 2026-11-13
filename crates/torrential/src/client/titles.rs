//! Films and series: finding one, showing it, listing its sources, and playing or downloading it
//! from the first of its sources that starts.

use std::time::Duration;

use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
use serde::Serialize;
use torrential_core::protocol::{GetReply, PlayReply, Request, Response, TitleFrame, TitlePlayed, TitleRequest};
use torrential_core::title::Episode;
use torrential_core::{AttemptEvent, Error, ErrorCode, Job, VideoRef};

use super::connect::{self, Conn};
use super::{answer, emit};
use crate::output::theme::{Token, paint};
use crate::output::{Mode, Typed, views};

/// The longest a title play goes between two steps: its whole budget, and the search before it.
const STEP_WITHIN: Duration = Duration::from_secs(240);

/// The film or episode a person named: a title ID, a video such as `tt0903747:3:5`, or a series
/// ID with an episode such as `s03e05` or `3x05`.
pub fn video(text: &str, episode: Option<&str>) -> Result<VideoRef, Error> {
    let video: VideoRef = text.trim().parse().map_err(|e: Error| {
        e.with_hint("run torrential titles <words> to find a title's ID, such as tt0063350")
            .with_next("torrential titles <words>")
    })?;
    let Some(word) = episode else { return Ok(video) };
    if video.episode.is_some() {
        return Err(Error::new(
            ErrorCode::Usage,
            format!("{text} names an episode already, so {word} is one too many"),
        ));
    }
    let episode = parse_episode(word).ok_or_else(|| {
        Error::new(ErrorCode::InvalidRef, format!("{word:?} is not an episode"))
            .with_hint("name an episode such as s03e05 or 3x05")
    })?;
    Ok(VideoRef { episode: Some(episode), ..video })
}

/// `s03e05` or `3x05`, in either case.
fn parse_episode(word: &str) -> Option<Episode> {
    let word = word.trim().to_ascii_lowercase();
    let (season, episode) = match word.strip_prefix('s') {
        Some(rest) => rest.split_once('e')?,
        None => word.split_once('x')?,
    };
    let number = |text: &str| text.bytes().all(|b| b.is_ascii_digit()).then(|| text.parse().ok()).flatten();
    Some(Episode { season: number(season)?, episode: number(episode)? })
}

/// The film or episode `words` name when the first is a title ID, as `tt0903747 s03e05`, or
/// `None` for any other words.
pub fn named(words: &[String]) -> Result<Option<VideoRef>, Error> {
    let [first, rest @ ..] = words else { return Ok(None) };
    if first.trim().parse::<VideoRef>().is_err() {
        return Ok(None);
    }
    match rest {
        [] => video(first, None).map(Some),
        [episode] => video(first, Some(episode)).map(Some),
        _ => Err(Error::new(ErrorCode::Usage, format!("{first} takes one episode, not {}", rest.join(" ")))
            .with_hint("name an episode such as tt0903747 s03e05")),
    }
}

/// The error for `--source` or an episode, given with a torrent.
pub fn for_a_title(what: &str) -> Error {
    Error::new(ErrorCode::Usage, format!("{what} goes with a title ID such as tt0063350"))
        .with_hint("run torrential titles <words> to find a title's ID")
        .with_next("torrential titles <words>")
}

/// Says what to run when a lookup found no such title or episode.
fn findable(error: Error, video: &VideoRef) -> Error {
    if error.code != ErrorCode::NotFound || error.hint.is_some() {
        return error;
    }
    match video.episode {
        Some(_) => error
            .with_hint(format!("run torrential title {} to list its episodes", video.title))
            .with_next(format!("torrential title {}", video.title)),
        None => {
            error.with_hint("run torrential titles <words> to find a title's ID").with_next("torrential titles <words>")
        }
    }
}

pub fn titles(mut conn: Conn, words: &[String], mode: Mode) -> Result<u8, Error> {
    let words = words.join(" ");
    match answer(conn.call(&Request::Titles { words: words.clone() })?)? {
        Response::Titles { titles } => {
            let doc = views::titles(&titles, &words);
            Ok(emit(mode, &doc, &Response::Titles { titles }))
        }
        other => Err(connect::unexpected(&other)),
    }
}

pub fn title(mut conn: Conn, video: VideoRef, mode: Mode) -> Result<u8, Error> {
    let request = Request::Title { id: video.title.clone() };
    match answer(conn.call(&request)?).map_err(|e| findable(e, &video))? {
        Response::Title { title } => Ok(emit(mode, &views::title(&title), &Response::Title { title })),
        other => Err(connect::unexpected(&other)),
    }
}

pub fn sources(mut conn: Conn, video: VideoRef, mode: Mode) -> Result<u8, Error> {
    let request = Request::Sources { video: video.clone() };
    match answer(conn.call(&request)?).map_err(|e| findable(e, &video))? {
        Response::Sources { sources } => {
            let doc = views::sources(&sources, crate::daemon::owner::now());
            Ok(emit(mode, &doc, &Typed { kind: "sources", body: &*sources }))
        }
        other => Err(connect::unexpected(&other)),
    }
}

/// Plays a film or an episode from the first of its sources that starts.
pub fn play(conn: &mut Conn, request: TitleRequest, mode: Mode) -> Result<(PlayReply, TitlePlayed), Error> {
    let video = request.video.clone();
    match steps(conn, &Request::PlayTitle(request), &video, mode, "Playing")? {
        Response::PlayTitle { reply, title } => Ok((*reply, *title)),
        other => Err(connect::unexpected(&other)),
    }
}

/// Downloads a film or an episode from the first of its sources that starts, and answers the exit
/// code and the download.
pub fn get(conn: &mut Conn, request: TitleRequest, mode: Mode) -> Result<(u8, Job), Error> {
    let video = request.video.clone();
    let (reply, title) = match steps(conn, &Request::GetTitle(request), &video, mode, "Downloading")? {
        Response::GetTitle { reply, title } => (*reply, *title),
        other => return Err(connect::unexpected(&other)),
    };
    #[derive(Serialize)]
    struct Got<'a> {
        reply: &'a GetReply,
        title: &'a TitlePlayed,
    }
    let doc = views::get_title(&reply, &title.name);
    let exit = emit(mode, &doc, &Typed { kind: "get", body: Got { reply: &reply, title: &title } });
    Ok((exit, reply.job))
}

/// Sends a title play and shows each of its steps until the reply. When no source of the title
/// is found, a search for its name comes next.
pub(super) fn steps(
    conn: &mut Conn,
    request: &Request,
    video: &VideoRef,
    mode: Mode,
    started: &str,
) -> Result<Response, Error> {
    conn.send(request)?;
    let shown = Shown::new(mode);
    let mut search = None;
    loop {
        match conn.read_within::<TitleFrame>(STEP_WITHIN)? {
            TitleFrame::Step { step } => {
                if let AttemptEvent::Resolved { video, title, .. } = &*step {
                    search = Some(views::search_for(&views::title_name(title, video.episode)));
                }
                shown.step(&step, started);
            }
            TitleFrame::Reply(reply) => {
                return answer(*reply).map_err(|e| match search {
                    Some(search) if e.code == ErrorCode::NoSource && e.next.is_none() => e.with_next(search),
                    _ => findable(e, video),
                });
            }
        }
    }
}

/// A title play's steps as they happen: a line each on stderr in text modes, and in a terminal a
/// spinner that says what the play waits for.
struct Shown {
    mode: Mode,
    bar: Option<ProgressBar>,
}

impl Shown {
    fn new(mode: Mode) -> Self {
        let bar = (mode == Mode::Human).then(|| {
            let bar = ProgressBar::with_draw_target(None, ProgressDrawTarget::stderr())
                .with_style(ProgressStyle::with_template("{spinner} {msg}").expect("the template is valid"));
            bar.set_message("Looking up the title");
            bar.enable_steady_tick(Duration::from_millis(100));
            bar
        });
        Self { mode, bar: bar.filter(|bar| !bar.is_hidden()) }
    }

    fn step(&self, step: &AttemptEvent, started: &str) {
        if self.mode == Mode::Json {
            return;
        }
        if let (AttemptEvent::Check { check }, Some(bar)) = (step, &self.bar) {
            bar.set_message(views::check(*check));
        }
        let Some(line) = views::step(step, started) else { return };
        let line = match step {
            AttemptEvent::Skipped { .. } => paint(Token::Muted, &line),
            AttemptEvent::Playing { .. } => paint(Token::Mint, &line),
            _ => line,
        };
        match &self.bar {
            Some(bar) => bar.println(line),
            None => anstream::eprintln!("{line}"),
        }
    }
}

impl Drop for Shown {
    fn drop(&mut self) {
        if let Some(bar) = &self.bar {
            bar.finish_and_clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(text: &str, episode: Option<&str>) -> Result<String, (ErrorCode, String)> {
        video(text, episode).map(|v| v.to_string()).map_err(|e| (e.code, e.message))
    }

    #[test]
    fn a_video_is_a_title_id_with_an_episode_written_any_way_a_person_types_it() {
        assert_eq!(read("tt0063350", None), Ok("tt0063350".into()));
        assert_eq!(read(" tt0903747:3:5 ", None), Ok("tt0903747:3:5".into()));
        assert_eq!(read("tt0903747", Some("s03e05")), Ok("tt0903747:3:5".into()));
        assert_eq!(read("tt0903747", Some("S3E5")), Ok("tt0903747:3:5".into()));
        assert_eq!(read("tt0903747", Some("3x05")), Ok("tt0903747:3:5".into()));
        assert_eq!(read("tt0903747", Some("s03")), Err((ErrorCode::InvalidRef, "\"s03\" is not an episode".into())));
        assert_eq!(read("tt0903747", Some("sxe")), Err((ErrorCode::InvalidRef, "\"sxe\" is not an episode".into())));
        assert_eq!(
            read("tt0903747:3:5", Some("s01e01")),
            Err((ErrorCode::Usage, "tt0903747:3:5 names an episode already, so s01e01 is one too many".into()))
        );
        assert_eq!(read("tt12", None), Err((ErrorCode::InvalidRef, "\"tt12\" is not a title ID".into())));
    }

    #[test]
    fn only_words_that_start_with_a_title_id_name_a_title() {
        let words = |text: &str| text.split(' ').map(String::from).collect::<Vec<_>>();
        let named = |text: &str| named(&words(text)).map(|v| v.map(|v| v.to_string())).map_err(|e| e.message);
        assert_eq!(named("tt0903747 s01e02"), Ok(Some("tt0903747:1:2".into())));
        assert_eq!(named("tt0063350"), Ok(Some("tt0063350".into())));
        assert_eq!(named("night of the living dead"), Ok(None));
        assert_eq!(named("6bc1af"), Ok(None));
        assert_eq!(named("tt0903747 s01e02 again"), Err("tt0903747 takes one episode, not s01e02 again".into()));
    }
}
