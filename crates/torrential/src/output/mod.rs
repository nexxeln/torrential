pub mod format;
pub mod theme;
pub mod views;

use serde::Serialize;
use torrential_core::Error;
use torrential_core::error::Details;
use torrential_core::protocol::SCHEMA_VERSION;
use torrential_core::text::sanitize;

use crate::env::Env;
use theme::{Token, paint};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Json,
    Plain,
    Human,
}

impl Mode {
    pub fn detect(json: bool, stdout_is_terminal: bool, env: &Env) -> Mode {
        if json {
            Mode::Json
        } else if stdout_is_terminal && env.var("TERM") != Some("dumb") && env.agent().is_none() {
            Mode::Human
        } else {
            Mode::Plain
        }
    }
}

#[derive(Clone, PartialEq, Debug)]
pub struct Doc {
    pub body: Body,
    pub summary: Vec<String>,
    pub next: Option<String>,
}

#[derive(Clone, PartialEq, Debug)]
pub enum Body {
    Table { header: Vec<&'static str>, rows: Vec<Vec<Cell>> },
    Fields { title: Vec<Cell>, fields: Vec<(&'static str, Cell)> },
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Cell {
    pub text: String,
    pub token: Token,
}

impl Cell {
    pub fn new(text: impl Into<String>) -> Self {
        Self::styled(text, Token::Text)
    }

    pub fn styled(text: impl Into<String>, token: Token) -> Self {
        Self { text: sanitize(&text.into()), token }
    }
}

impl Doc {
    pub fn plain(&self) -> String {
        let mut out = String::new();
        match &self.body {
            Body::Table { header, rows } => {
                if !header.is_empty() {
                    push_line(&mut out, &header.join("\t"));
                }
                for row in rows {
                    let cells: Vec<&str> = row.iter().map(|c| c.text.as_str()).collect();
                    push_line(&mut out, &cells.join("\t"));
                }
                self.push_summary(&mut out, |s| format!("# {s}"));
                if let Some(next) = self.next() {
                    push_line(&mut out, &format!("next: {next}"));
                }
            }
            Body::Fields { title, fields } => {
                if !title.is_empty() {
                    let words: Vec<&str> = title.iter().map(|c| c.text.as_str()).collect();
                    push_line(&mut out, &words.join(" "));
                }
                let width = fields.iter().map(|(k, _)| k.len()).chain([4]).max().unwrap_or(0) + 2;
                for (key, value) in fields {
                    push_line(&mut out, &format!("{:width$}{}", format!("{key}:"), value.text));
                }
                self.push_summary(&mut out, |s| format!("# {s}"));
                if let Some(next) = self.next() {
                    push_line(&mut out, &format!("{:width$}{next}", "next:"));
                }
            }
        }
        out
    }

    pub fn human(&self) -> String {
        let mut out = String::new();
        match &self.body {
            Body::Table { header, rows } => {
                let mut widths: Vec<usize> = header.iter().map(|h| h.chars().count()).collect();
                for row in rows {
                    for (w, cell) in widths.iter_mut().zip(row) {
                        *w = (*w).max(cell.text.chars().count());
                    }
                }
                let heading: Vec<Cell> = header.iter().map(|h| Cell::styled(*h, Token::Heading)).collect();
                push_line(&mut out, &aligned(&heading, &widths));
                for row in rows {
                    push_line(&mut out, &aligned(row, &widths));
                }
            }
            Body::Fields { title, fields } => {
                if !title.is_empty() {
                    let words: Vec<String> = title.iter().map(|c| paint(c.token, &c.text)).collect();
                    push_line(&mut out, &words.join(" "));
                }
                let width = fields.iter().map(|(k, _)| k.len()).max().unwrap_or(0) + 2;
                for (key, value) in fields {
                    let key = format!("{:width$}", format!("{key}:"));
                    push_line(&mut out, &format!("{}{}", paint(Token::Muted, &key), paint(value.token, &value.text)));
                }
            }
        }
        self.push_summary(&mut out, |s| paint(Token::Muted, s));
        if let Some(next) = self.next() {
            push_line(&mut out, &paint(Token::Muted, &format!("next: {next}")));
        }
        out
    }

    fn push_summary(&self, out: &mut String, line: impl Fn(&str) -> String) {
        for s in &self.summary {
            push_line(out, &line(&sanitize(s)));
        }
    }

    fn next(&self) -> Option<String> {
        self.next.as_deref().map(sanitize)
    }
}

fn aligned(cells: &[Cell], widths: &[usize]) -> String {
    let last = cells.len().saturating_sub(1);
    let parts: Vec<String> = cells
        .iter()
        .zip(widths)
        .enumerate()
        .map(|(i, (cell, width))| {
            let pad = if i == last { 0 } else { width - cell.text.chars().count() };
            format!("{}{}", paint(cell.token, &cell.text), " ".repeat(pad))
        })
        .collect();
    parts.join("  ")
}

fn push_line(out: &mut String, line: &str) {
    out.push_str(line);
    out.push('\n');
}

pub fn json_document(payload: &impl Serialize, next: Option<&str>) -> String {
    let mut map = json_fields(payload);
    map.insert("next".into(), next.into());
    finish_json(map)
}

pub fn json_line(payload: &impl Serialize) -> String {
    finish_json(json_fields(payload))
}

fn json_fields(payload: &impl Serialize) -> serde_json::Map<String, serde_json::Value> {
    let mut map = serde_json::Map::new();
    map.insert("schema".into(), SCHEMA_VERSION.into());
    match serde_json::to_value(payload).expect("output types serialize") {
        serde_json::Value::Object(fields) => map.extend(fields),
        other => {
            map.insert("data".into(), other);
        }
    }
    map
}

fn finish_json(map: serde_json::Map<String, serde_json::Value>) -> String {
    let mut text = serde_json::to_string(&map).expect("a map serializes");
    text.push('\n');
    text
}

#[derive(Serialize)]
pub struct Typed<T: Serialize> {
    #[serde(rename = "type")]
    pub kind: &'static str,
    #[serde(flatten)]
    pub body: T,
}

pub fn render(mode: Mode, doc: &Doc, payload: &impl Serialize) -> String {
    match mode {
        Mode::Json => json_document(payload, doc.next.as_deref()),
        Mode::Plain => doc.plain(),
        Mode::Human => doc.human(),
    }
}

#[derive(Serialize)]
struct ErrorDocument<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    error: &'a Error,
}

pub struct ErrorOutput {
    pub stdout: String,
    pub stderr: String,
}

pub fn error(mode: Mode, error: &Error) -> ErrorOutput {
    match mode {
        Mode::Json => {
            let mut error = error.clone();
            let next = error.next.take();
            let document = ErrorDocument { kind: "error", error: &error };
            ErrorOutput { stdout: json_document(&document, next.as_deref().map(String::as_str)), stderr: String::new() }
        }
        Mode::Plain => ErrorOutput { stdout: String::new(), stderr: error_text(error, false) },
        Mode::Human => ErrorOutput { stdout: String::new(), stderr: error_text(error, true) },
    }
}

fn error_text(error: &Error, color: bool) -> String {
    let paint = |token, s: &str| if color { paint(token, s) } else { s.to_string() };
    let mut out = String::new();
    let head = format!("error[{}]:", error.code);
    push_line(&mut out, &format!("{} {}", paint(Token::Error, &head), sanitize(&error.message)));
    for line in detail_lines(error.details.as_ref()) {
        push_line(&mut out, &format!("  {line}"));
    }
    if let Some(hint) = &error.hint {
        push_line(&mut out, &paint(Token::Muted, &format!("hint: {}", sanitize(hint))));
    }
    if let Some(next) = &error.next {
        push_line(&mut out, &paint(Token::Muted, &format!("next: {}", sanitize(next))));
    }
    out
}

fn detail_lines(details: Option<&Details>) -> Vec<String> {
    match details {
        None => Vec::new(),
        Some(Details::Candidates(candidates)) => {
            candidates.iter().map(|c| format!("{}\t{}\t{}", c.id, sanitize(&c.name), c.kind)).collect()
        }
        Some(Details::Sources(reports)) => {
            reports.iter().map(|r| format!("{}: {}", r.source, views::source_outcome(&r.outcome))).collect()
        }
        Some(Details::Space { .. }) => Vec::new(),
        Some(Details::Sandbox { signal, harness }) => match harness {
            Some(harness) => vec![format!("signal: {signal} ({harness})")],
            None => vec![format!("signal: {signal}")],
        },
        Some(Details::Versions { client, daemon }) => vec![format!("client {client}, engine {daemon}")],
        Some(Details::Paths(paths)) => paths.iter().map(|p| sanitize(p)).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use torrential_core::ErrorCode;

    fn env(vars: &[(&str, &str)]) -> Env {
        Env::from_pairs(vars.iter().copied())
    }

    #[test]
    fn mode_detection_table() {
        type Case = (bool, bool, &'static [(&'static str, &'static str)], Mode);
        let cases: [Case; 7] = [
            (true, true, &[], Mode::Json),
            (false, true, &[("TERM", "xterm-256color")], Mode::Human),
            (false, false, &[("TERM", "xterm-256color")], Mode::Plain),
            (false, true, &[("TERM", "dumb")], Mode::Plain),
            (false, true, &[("CLAUDECODE", "1")], Mode::Plain),
            (false, true, &[("CODEX_THREAD_ID", "t-1")], Mode::Plain),
            (false, true, &[("AI_AGENT", "")], Mode::Human),
        ];
        for (json, tty, vars, expected) in cases {
            assert_eq!(Mode::detect(json, tty, &env(vars)), expected, "json={json} tty={tty} vars={vars:?}");
        }
    }

    #[test]
    fn json_documents_lead_with_the_schema_and_end_with_next() {
        let doc = json_document(&serde_json::json!({"type": "ok"}), Some("torrential status"));
        assert_eq!(doc, "{\"schema\":1,\"type\":\"ok\",\"next\":\"torrential status\"}\n");
    }

    #[test]
    fn plain_errors_go_to_stderr_with_the_hint_last() {
        let e = Error::new(ErrorCode::NotFound, "no download matches 9b52").with_hint("torrential status");
        let out = error(Mode::Plain, &e);
        assert_eq!(out.stdout, "");
        assert_eq!(out.stderr, "error[not_found]: no download matches 9b52\nhint: torrential status\n");
    }

    #[test]
    fn an_error_whose_hint_leads_with_a_command_names_it_as_next() {
        let e = Error::new(ErrorCode::VerificationFailed, "3 damaged pieces in the selected files")
            .with_hint("torrential verify 6bc1af --repair downloads them again")
            .with_next("torrential verify 6bc1af --repair");
        assert_eq!(
            error(Mode::Plain, &e).stderr,
            "error[verification_failed]: 3 damaged pieces in the selected files\nhint: torrential verify 6bc1af --repair \
             downloads them again\nnext: torrential verify 6bc1af --repair\n"
        );
        let json: serde_json::Value = serde_json::from_str(&error(Mode::Json, &e).stdout).unwrap();
        assert_eq!(json["next"], "torrential verify 6bc1af --repair");
        let keys: Vec<&String> = json["error"].as_object().unwrap().keys().collect();
        assert_eq!(keys, ["code", "message", "hint"]);
    }

    #[test]
    fn a_path_in_a_hint_stays_on_the_hint_line() {
        let e = Error::new(ErrorCode::StorageError, "cannot write SKILL.md")
            .with_hint("run it in a terminal: cd '/src/my\nrepo' && torrential skill install");
        assert_eq!(
            error(Mode::Plain, &e).stderr,
            "error[storage_error]: cannot write SKILL.md\nhint: run it in a terminal: cd '/src/my repo' && torrential \
             skill install\n"
        );
    }
}
