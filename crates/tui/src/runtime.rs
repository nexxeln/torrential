use std::io::{self, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossterm::event::Event as TermEvent;
use torrential_core::protocol::Event;
use torrential_core::{Error, ErrorCode};

use crate::backend::Backend;
use crate::state::{JobAction, State, Tone, Upgrade};
use crate::theme::ColorMode;
use crate::update::{Effect, Msg};
use crate::view;

const NUMBERS_EVERY: Duration = Duration::from_millis(500);
const WATCH_RETRY: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Exit {
    Quit,
    /// A newer torrential replaced this binary, so the caller starts it in place of this process.
    Restart,
}

/// Runs the TUI until the user quits. Downloads keep running in the engine afterwards.
pub fn run(backend: impl Backend) -> io::Result<Exit> {
    let mode = ColorMode::detect(|key| std::env::var(key).ok());
    let mut terminal = ratatui::try_init()?;
    let result = session(&mut terminal, Arc::new(backend), mode);
    ratatui::restore();
    let state = result?;
    if state.upgrade == Some(Upgrade::Installed) {
        return Ok(Exit::Restart);
    }
    match state.active() {
        0 => {}
        1 => println!("1 download continues in the background."),
        n => println!("{n} downloads continue in the background."),
    }
    Ok(Exit::Quit)
}

fn session<B: Backend>(terminal: &mut ratatui::DefaultTerminal, backend: Arc<B>, mode: ColorMode) -> io::Result<State> {
    let size = terminal.size()?;
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut state = State::new(Instant::now(), unix_now(), (size.width, size.height), home);
    state.say(Tone::Info, "Downloads keep running after you quit.");
    terminal.draw(|frame| draw(frame, &state, mode))?;
    let (tx, rx) = mpsc::channel();
    listen(tx.clone());
    watch(backend.clone(), tx.clone());
    let (reader, sender) = (backend.clone(), tx.clone());
    thread::spawn(move || {
        if let Ok(preferences) = reader.preferences() {
            let _ = sender.send(Msg::Preferences(preferences));
        }
        if let Some(machine) = reader.machine() {
            let _ = sender.send(Msg::Machine(machine));
        }
        if let Some(latest) = reader.newer_version() {
            let _ = sender.send(Msg::NewerVersion(latest));
        }
    });
    run_loop(terminal, &mut state, &rx, &tx, &backend, mode)?;
    Ok(state)
}

fn draw(frame: &mut ratatui::Frame, state: &State, mode: ColorMode) {
    if let Some(at) = view::render(frame.buffer_mut(), state, mode) {
        frame.set_cursor_position(at);
    }
}

fn unix_now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

fn urgency(state: &State, msg: &Msg) -> Option<Duration> {
    match msg {
        Msg::Watch(Event::Job { job }) if state.job(job.id).is_some_and(|j| j.status == job.status) => {
            Some(NUMBERS_EVERY)
        }
        Msg::Watch(Event::Heartbeat { .. }) => None,
        _ => Some(Duration::ZERO),
    }
}

fn run_loop<B: Backend>(
    terminal: &mut ratatui::DefaultTerminal,
    state: &mut State,
    rx: &mpsc::Receiver<Msg>,
    tx: &Sender<Msg>,
    backend: &Arc<B>,
    mode: ColorMode,
) -> io::Result<()> {
    let mut drawn = Instant::now();
    let mut due: Option<Instant> = None;
    loop {
        let wake = [due, wake_at(state)].into_iter().flatten().min();
        let first = match wake {
            Some(at) => match rx.recv_timeout(at.saturating_duration_since(Instant::now())) {
                Ok(msg) => msg,
                Err(RecvTimeoutError::Timeout) => Msg::Tick,
                Err(RecvTimeoutError::Disconnected) => return Ok(()),
            },
            None => match rx.recv() {
                Ok(msg) => msg,
                Err(_) => return Ok(()),
            },
        };
        let mut next = Some(first);
        while let Some(msg) = next {
            state.now = Instant::now();
            state.clock = unix_now();
            if let Some(after) = urgency(state, &msg) {
                let at = if after.is_zero() { state.now } else { (drawn + after).max(state.now) };
                due = Some(due.map_or(at, |d| d.min(at)));
            }
            for effect in state.update(msg) {
                perform(effect, backend, tx);
            }
            if state.quit {
                return Ok(());
            }
            next = rx.try_recv().ok();
        }
        if due.is_some_and(|at| at <= Instant::now()) {
            terminal.draw(|frame| draw(frame, state, mode))?;
            drawn = Instant::now();
            due = None;
        }
    }
}

fn wake_at(state: &State) -> Option<Instant> {
    let spin = state.spinning().then(|| state.next_frame());
    let toast = state.toast.as_ref().map(|t| t.until);
    let prefetch = state.prefetch.map(|(_, at)| at);
    [spin, toast, prefetch, state.next_stall()].into_iter().flatten().min()
}

fn listen(tx: Sender<Msg>) {
    thread::spawn(move || {
        loop {
            let msg = match crossterm::event::read() {
                Ok(TermEvent::Key(key)) => Msg::Key(key),
                Ok(TermEvent::Resize(w, h)) => Msg::Resize(w, h),
                Ok(_) => continue,
                Err(_) => return,
            };
            if tx.send(msg).is_err() {
                return;
            }
        }
    });
}

fn watch<B: Backend>(backend: Arc<B>, tx: Sender<Msg>) {
    thread::spawn(move || {
        loop {
            let result = backend.watch(&mut |event| {
                if !matches!(event, Event::Heartbeat { .. }) {
                    let _ = tx.send(Msg::Watch(event));
                }
            });
            let error = result
                .err()
                .unwrap_or_else(|| Error::new(ErrorCode::DaemonUnavailable, "the engine closed the connection"));
            if tx.send(Msg::WatchLost(error)).is_err() {
                return;
            }
            thread::sleep(WATCH_RETRY);
        }
    });
}

fn perform<B: Backend>(effect: Effect, backend: &Arc<B>, tx: &Sender<Msg>) {
    let (backend, tx) = (backend.clone(), tx.clone());
    match effect {
        Effect::Search { search, query } => {
            thread::spawn(move || {
                let result = backend.search(query, &mut |result, pending| {
                    let _ = tx.send(Msg::SearchUpdate { search, result, pending });
                });
                let _ = tx.send(Msg::SearchDone { search, result });
            });
        }
        Effect::Files(item) => {
            thread::spawn(move || {
                let _ = tx.send(Msg::Files { item, result: backend.files(&item.reference()) });
            });
        }
        Effect::Get { release, files } => {
            thread::spawn(move || {
                let result = backend.get(&release.to_ref(), files).map(Box::new);
                let _ = tx.send(Msg::Started { release, result });
            });
        }
        Effect::Play(item) => {
            thread::spawn(move || {
                let _ = tx.send(Msg::Played(backend.play(&item.reference())));
            });
        }
        Effect::Job { id, action } => {
            thread::spawn(move || {
                let hex = id.to_hex();
                let result = match action {
                    JobAction::Pause => backend.pause(&hex),
                    JobAction::Resume => backend.resume(&hex),
                    JobAction::Verify => backend.verify(&hex),
                    JobAction::Remove => backend.remove(&hex),
                };
                let _ = tx.send(Msg::Changed { id, action, result });
            });
        }
        Effect::FreeSpace => {
            thread::spawn(move || {
                let _ = tx.send(Msg::FreeSpace(backend.free_space()));
            });
        }
        Effect::Open(path) => {
            let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
            let _ =
                Command::new(opener).arg(path).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
        }
        Effect::Copy(text) => copy(&text),
        Effect::SavePreferences(edit) => {
            thread::spawn(move || {
                let result = backend.save_preferences(edit);
                let _ = tx.send(Msg::Saved { edit, result });
            });
        }
        Effect::Upgrade => {
            thread::spawn(move || {
                let _ = tx.send(Msg::Upgraded(backend.upgrade()));
            });
        }
        Effect::Home { ask } => {
            thread::spawn(move || {
                let _ = tx.send(Msg::Home { ask, result: backend.home() });
            });
        }
        Effect::FindTitles { search, words } => {
            thread::spawn(move || {
                let _ = tx.send(Msg::FoundTitles { search, result: backend.titles(&words) });
            });
        }
        Effect::Title { ask, id } => {
            thread::spawn(move || {
                let result = backend.title(&id).map(Box::new);
                let _ = tx.send(Msg::Title { ask, result });
            });
        }
        Effect::PlayTitle(video) => {
            thread::spawn(move || {
                let result = backend.play_title(&video, &mut |step| {
                    let _ = tx.send(Msg::PlayStep(Box::new(step)));
                });
                let _ = tx.send(Msg::TitlePlayed(result.map(|(player, played)| (player, Box::new(played)))));
            });
        }
    }
}

/// `pbcopy` on macOS. Elsewhere an OSC 52 sequence asks the terminal to set the clipboard.
fn copy(text: &str) {
    if cfg!(target_os = "macos")
        && let Ok(mut child) = Command::new("pbcopy").stdin(Stdio::piped()).stdout(Stdio::null()).spawn()
    {
        if let Some(stdin) = child.stdin.as_mut() {
            let _ = stdin.write_all(text.as_bytes());
        }
        let _ = child.wait();
        return;
    }
    let mut out = io::stdout();
    let _ = write!(out, "\x1b]52;c;{}\x07", base64(text.as_bytes()));
    let _ = out.flush();
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |n, (i, b)| n | u32::from(*b) << (16 - 8 * i));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::base64;

    #[test]
    fn base64_pads_short_input() {
        assert_eq!([base64(b"path"), base64(b"pa"), base64(b"p")], ["cGF0aA==", "cGE=", "cA=="]);
    }
}
