//! `torrential pull`: copies a finished download's files from the engine on another machine to
//! this one through the engine's file route, and goes on from where a cut transfer stopped.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
use serde::Serialize;
use torrential_core::protocol::Request;
use torrential_core::units::format_size;
use torrential_core::{Error, ErrorCode, Job, JobStatus, Target};

use super::remote::{Remote, Remotes};
use super::{connect, emit, file_list, files};
use crate::cli::PullArgs;
use crate::output::theme::Token;
use crate::output::{Body, Cell, Doc, DocKind, Mode, Typed};
use torrential_daemon::config::{self, Config};
use torrential_daemon::daemon::devices::COOKIE;
use torrential_daemon::env::Env;
use torrential_daemon::paths::Paths;
use torrential_daemon::wire::{self, Address};

/// How long a read of the file may wait before the transfer counts as cut.
const READ_WITHIN: Duration = Duration::from_secs(30);
/// Cut transfers in a row with nothing received, after which a pull gives up until it runs again.
const TRIES: u32 = 4;
const BUFFER: usize = 1 << 20;

/// A file this pull copied.
#[derive(Serialize)]
struct Pulled {
    /// Where it is on this machine.
    path: PathBuf,
    size: u64,
    /// What an earlier, cut pull had already copied, which this one went on from.
    resumed_at: u64,
}

pub fn run(args: PullArgs, mode: Mode, env: &Env, cwd: &Path) -> Result<u8, Error> {
    let paths = Paths::from_process(env)?;
    let remote = Remotes::using(&paths)?.ok_or_else(|| Error::internal("no engine on another machine is chosen"))?;
    let mut conn = connect::remote(&remote, env)?;
    let job = done(&mut conn, &args.reference)?;
    let mut listing = file_list(&mut conn, Target::Prefix(job.id.to_hex()))?;
    listing.files.retain(|file| file.selected);
    if !args.files.is_empty() {
        files::keep_matching(&mut listing, &args.files)?;
    }
    let dir = match &args.dir {
        Some(dir) => super::absolute(dir, cwd, env),
        None => config::downloads_dir(Config::load(&paths, env).as_ref(), &paths),
    };
    let name = listing.name.clone().unwrap_or_else(|| job.id.to_hex());
    let total = listing.files.iter().map(|file| file.size).sum();
    let bar = progress(mode, total);
    let mut pulled = Vec::new();
    for file in &listing.files {
        let path = inside(&dir, &name, &file.path).ok_or_else(|| {
            Error::new(
                ErrorCode::InvalidTorrent,
                format!("the engine named a file outside the download: {}", file.path),
            )
        })?;
        let route = wire::file_route(job.id, file.index, &file.path);
        let resumed_at = copy(&remote, &route, file.size, &path, &bar)?;
        pulled.push(Pulled { path, size: file.size, resumed_at });
    }
    bar.finish_and_clear();
    #[derive(Serialize)]
    struct Payload<'a> {
        id: String,
        files: &'a [Pulled],
    }
    let doc = shown(&pulled, &remote.address);
    Ok(emit(mode, &doc, &Typed { kind: DocKind::Pull, body: Payload { id: job.id.to_hex(), files: &pulled } }))
}

/// The download `reference` names, once it is done.
fn done(conn: &mut connect::Conn, reference: &str) -> Result<Job, Error> {
    let jobs: Vec<Job> = conn.ask(&Request::Status { refs: vec![reference.to_string()] })?;
    let job =
        jobs.into_iter().next().ok_or_else(|| Error::new(ErrorCode::NotFound, format!("no download {reference}")))?;
    match job.status {
        JobStatus::Done { .. } => Ok(job),
        _ => Err(Error::new(ErrorCode::JobConflict, format!("the download {reference} is not done yet"))
            .with_hint(format!("run torrential wait {reference}, then pull it again"))
            .with_next(format!("torrential wait {reference}"))),
    }
}

/// Where file `path` of the download `name` goes in `dir`, laid out as the engine lays it out: a
/// torrent of one file is that file, and any other is a folder. `None` for a name or path that
/// would leave `dir`, which an engine that misbehaves could send.
fn inside(dir: &Path, name: &str, path: &str) -> Option<PathBuf> {
    let parts = |text: &str| -> Option<Vec<String>> {
        let parts: Vec<String> = Path::new(text)
            .components()
            .map(|part| match part {
                Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
                _ => None,
            })
            .collect::<Option<_>>()?;
        (!parts.is_empty()).then_some(parts)
    };
    let mut parts = if path == name { parts(path)? } else { [parts(name)?, parts(path)?].concat() };
    if parts.iter().any(|part| part.contains(['/', '\\'])) {
        return None;
    }
    let file = parts.pop()?;
    Some(parts.iter().fold(dir.to_path_buf(), |path, part| path.join(part)).join(file))
}

fn progress(mode: Mode, total: u64) -> ProgressBar {
    if mode != Mode::Human {
        return ProgressBar::hidden();
    }
    let style = ProgressStyle::with_template(&format!(
        "{{bar:32.{}/{}}} {{bytes}} of {{total_bytes}} · {{bytes_per_sec}} · {{eta}} left",
        Token::Mint.bar_color(),
        Token::Muted.bar_color(),
    ))
    .expect("the template is valid");
    ProgressBar::with_draw_target(Some(total), ProgressDrawTarget::stderr()).with_style(style)
}

/// Copies the `size` bytes of the file at `route` to `path` through `path.part`, going on from
/// what an earlier pull left there, and returns how much that was. A file already at `path` at
/// its full size stays as it is.
fn copy(remote: &Remote, route: &str, size: u64, path: &Path, bar: &ProgressBar) -> Result<u64, Error> {
    let failed = |e: std::io::Error, what: &Path| {
        Error::new(ErrorCode::StorageError, format!("cannot write {}: {e}", what.display()))
    };
    match std::fs::metadata(path) {
        Ok(found) if found.len() == size => {
            bar.inc(size);
            return Ok(size);
        }
        Ok(_) => {
            return Err(Error::new(
                ErrorCode::PathConflict,
                format!("{} is there already, at another size", path.display()),
            )
            .with_hint("move it away, then pull again"));
        }
        Err(_) => {}
    }
    let part = partial(path);
    if let Some(parent) = part.parent() {
        std::fs::create_dir_all(parent).map_err(|e| failed(e, parent))?;
    }
    let mut out =
        OpenOptions::new().create(true).write(true).truncate(false).open(&part).map_err(|e| failed(e, &part))?;
    let length = |out: &File| out.metadata().map(|meta| meta.len()).map_err(|e| failed(e, &part));
    let resumed_at = length(&out)?.min(size);
    bar.inc(resumed_at);
    let mut idle = 0;
    loop {
        let have = length(&out)?;
        if have == size {
            break;
        }
        let have = if have > size { 0 } else { have };
        match fetch(&remote.address, &remote.token, route, have, &mut out, bar) {
            Ok(received) if received > 0 => idle = 0,
            Ok(_) => {
                idle += 1;
                if idle >= TRIES {
                    return Err(Error::new(
                        ErrorCode::DaemonUnavailable,
                        format!("the engine at {} stopped sending {}", remote.address, path.display()),
                    )
                    .with_hint("run torrential pull again, and it goes on from where it stopped"));
                }
                std::thread::sleep(Duration::from_millis(500 << idle));
            }
            Err(error) => return Err(error),
        }
    }
    drop(out);
    std::fs::rename(&part, path).map_err(|e| failed(e, path))?;
    Ok(resumed_at)
}

/// `name.part` beside `path`, where the file grows until it is whole.
fn partial(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    path.with_file_name(name)
}

/// Asks for the file at `route` from byte `have` on, writes what arrives to `out` from there, and
/// returns how many bytes that was. A connection that fails or ends early returns what came.
fn fetch(
    address: &Address,
    token: &str,
    route: &str,
    have: u64,
    out: &mut File,
    bar: &ProgressBar,
) -> Result<u64, Error> {
    let cookie = format!("{COOKIE}={token}");
    let range = format!("bytes={have}-");
    let mut headers = vec![("Cookie", cookie.as_str())];
    if have > 0 {
        headers.push(("Range", range.as_str()));
    }
    let Ok((head, mut body)) = wire::exchange(address, "GET", route, &headers, &[], READ_WITHIN) else { return Ok(0) };
    let start = match head.status {
        200 => 0,
        206 => {
            let start = head
                .header("content-range")
                .and_then(|range| range.strip_prefix("bytes ")?.split('-').next()?.parse::<u64>().ok());
            start
                .filter(|start| *start <= have)
                .ok_or_else(|| Error::internal("the engine sent a part of the file this torrential did not ask for"))?
        }
        401 | 303 => {
            return Err(Error::new(
                ErrorCode::NotPaired,
                format!("this machine is no longer paired with the engine at {address}"),
            )
            .with_hint(format!(
                "run torrential devices add on that machine, then torrential connect {address} --code <code>"
            )));
        }
        status => {
            return Err(Error::new(
                ErrorCode::DaemonUnavailable,
                format!("the engine at {address} answered HTTP {status} for the file"),
            ));
        }
    };
    let storage = |e: std::io::Error| Error::new(ErrorCode::StorageError, format!("cannot write the file: {e}"));
    out.set_len(start).and_then(|()| out.seek(SeekFrom::Start(start))).map_err(storage)?;
    bar.set_position(bar.position().saturating_sub(have - start));
    let mut buf = vec![0; BUFFER];
    let mut received = 0;
    // A read that fails is a cut connection, which the next request goes on from.
    while let Ok(read @ 1..) = body.read(&mut buf) {
        out.write_all(&buf[..read]).map_err(storage)?;
        received += read as u64;
        bar.inc(read as u64);
    }
    out.flush().map_err(storage)?;
    Ok(received)
}

fn shown(pulled: &[Pulled], from: &Address) -> Doc {
    let rows = pulled
        .iter()
        .map(|file| {
            let resumed = if file.resumed_at > 0 && file.resumed_at < file.size {
                format!("went on from {}", format_size(file.resumed_at))
            } else {
                String::new()
            };
            vec![
                Cell::styled(file.path.display().to_string(), Token::Accent),
                Cell::new(format_size(file.size)),
                Cell::new(resumed),
            ]
        })
        .collect();
    Doc {
        body: Body::Table { header: vec!["PATH", "SIZE", "NOTE"], rows },
        summary: vec![format!("copied from the engine at {from}")],
        next: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn a_file_lands_under_the_downloads_folder_and_never_outside_it() {
        let dir = Path::new("/home/ada/Downloads");
        let cases = [
            ("Sintel.mkv", "Sintel.mkv", Some("/home/ada/Downloads/Sintel.mkv")),
            ("Sintel", "Sintel.mkv", Some("/home/ada/Downloads/Sintel/Sintel.mkv")),
            ("Sintel", "subs/en.srt", Some("/home/ada/Downloads/Sintel/subs/en.srt")),
            ("Sintel", "../.ssh/authorized_keys", None),
            ("..", "authorized_keys", None),
            ("Sintel", "/etc/passwd", None),
            ("Sintel", "", None),
            ("", "Sintel.mkv", None),
        ];
        for (name, path, expected) in cases {
            assert_eq!(inside(dir, name, path), expected.map(PathBuf::from), "{name} {path}");
        }
    }

    /// An engine's file route that serves `bytes`, cutting its first answer after `cut` bytes,
    /// and records the `Range` of each request.
    fn engine(bytes: Vec<u8>, cut: Option<usize>) -> (Address, std::thread::JoinHandle<Vec<Option<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = Address::local(listener.local_addr().unwrap().port());
        let served = std::thread::spawn(move || {
            let mut ranges = Vec::new();
            for (n, stream) in listener.incoming().enumerate() {
                let mut stream = stream.unwrap();
                let mut head = Vec::new();
                let mut byte = [0u8];
                while !head.ends_with(b"\r\n\r\n") && stream.read(&mut byte).unwrap() == 1 {
                    head.push(byte[0]);
                }
                let head = String::from_utf8(head).unwrap();
                assert!(head.contains(&format!("Cookie: {COOKIE}={}", "ab".repeat(32))), "{head}");
                let range = head.lines().find_map(|line| line.strip_prefix("Range: bytes=")).map(String::from);
                let start: usize = range.as_deref().map_or(0, |range| range.trim_end_matches('-').parse().unwrap());
                ranges.push(range);
                let (status, extra) = if start > 0 {
                    (
                        "206 Partial Content",
                        format!("Content-Range: bytes {start}-{}/{}\r\n", bytes.len() - 1, bytes.len()),
                    )
                } else {
                    ("200 OK", String::new())
                };
                let answer = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\n{extra}Connection: close\r\n\r\n",
                    bytes.len() - start
                );
                stream.write_all(answer.as_bytes()).unwrap();
                let end = match (n, cut) {
                    (0, Some(cut)) => cut,
                    _ => bytes.len(),
                };
                stream.write_all(&bytes[start..end]).unwrap();
                if end == bytes.len() {
                    return ranges;
                }
            }
            ranges
        });
        (address, served)
    }

    fn film() -> Vec<u8> {
        (0..3_000_000u32).map(|n| (n % 251) as u8).collect()
    }

    #[test]
    fn a_cut_transfer_goes_on_from_where_it_stopped_and_the_file_appears_only_when_whole() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Sintel.mkv");
        let bytes = film();
        let (address, served) = engine(bytes.clone(), Some(1_200_000));
        let remote = Remote { address, token: "ab".repeat(32), device: "3f9a0c2e".into() };

        let resumed_at =
            copy(&remote, "/9b52/0/Sintel.mkv", bytes.len() as u64, &path, &ProgressBar::hidden()).unwrap();

        assert_eq!(served.join().unwrap(), [None, Some("1200000-".to_string())]);
        assert_eq!((resumed_at, std::fs::read(&path).unwrap() == bytes), (0, true));
        assert!(!partial(&path).exists());
    }

    #[test]
    fn a_pull_that_was_killed_goes_on_from_the_part_file_it_left() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Sintel.mkv");
        let bytes = film();
        std::fs::write(partial(&path), &bytes[..700_000]).unwrap();
        let (address, served) = engine(bytes.clone(), None);
        let remote = Remote { address, token: "ab".repeat(32), device: "3f9a0c2e".into() };

        let resumed_at =
            copy(&remote, "/9b52/0/Sintel.mkv", bytes.len() as u64, &path, &ProgressBar::hidden()).unwrap();

        assert_eq!(served.join().unwrap(), [Some("700000-".to_string())]);
        assert_eq!((resumed_at, std::fs::read(&path).unwrap() == bytes), (700_000, true));
    }
}
