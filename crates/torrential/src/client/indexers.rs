//! `torrential indexers`: the Torznab indexers the engine searches, such as the trackers added to
//! Jackett or Prowlarr.

use torrential_core::Error;
use torrential_core::indexer::{ApiKey, Check, Indexer, IndexerList, KeyFrom};
use torrential_core::protocol::Request;
use torrential_core::units::count;

use super::connect::Conn;
use super::emit;
use crate::cli::IndexersCommand;
use crate::output::theme::Token;
use crate::output::{Body, Cell, Doc, DocKind, Mode, Typed};

/// How many category names a row shows.
const CATEGORIES_SHOWN: usize = 4;

pub fn run(mut conn: Conn, command: Option<IndexersCommand>, mode: Mode) -> Result<u8, Error> {
    let request = match command {
        None | Some(IndexersCommand::List) => Request::Indexers,
        Some(IndexersCommand::Add { url, name, key }) => {
            Request::AddIndexer { url, name, key: key.as_deref().and_then(ApiKey::new) }
        }
        Some(IndexersCommand::Remove { name }) => Request::RemoveIndexer { name },
    };
    let list: IndexerList = conn.ask(&request)?;
    Ok(emit(mode, &list_doc(&list), &Typed { kind: DocKind::Indexers, body: &list }))
}

fn list_doc(list: &IndexerList) -> Doc {
    let rows = list.indexers.iter().map(row).collect();
    let mut summary = vec![count(list.indexers.len(), "indexer")];
    if !list.added.is_empty() {
        summary.push(format!("added {}", list.added.join(", ")));
    }
    for Indexer { name, check, .. } in &list.indexers {
        if let Check::Fails { reason } = check {
            summary.push(format!("{name} does not answer: {reason}"));
        }
    }
    if list.indexers.is_empty() {
        summary.push("add one with torrential indexers add <address> --key <key>".into());
    }
    let header = vec!["NAME", "ANSWERS", "KEY", "CATEGORIES", "URL"];
    Doc { body: Body::Table { header, rows }, summary, next: None }
}

fn row(indexer: &Indexer) -> Vec<Cell> {
    let (answers, categories) = match &indexer.check {
        Check::Answers { categories } => {
            let mut names: Vec<&str> = categories.iter().take(CATEGORIES_SHOWN).map(|c| c.name.as_str()).collect();
            let more = categories.len().saturating_sub(CATEGORIES_SHOWN);
            let more = format!("+{more}");
            if names.len() < categories.len() {
                names.push(&more);
            }
            let shown = if names.is_empty() { "-".to_string() } else { names.join(", ") };
            (Cell::styled("yes", Token::Mint), Cell::new(shown))
        }
        Check::Fails { .. } => (Cell::styled("no", Token::Error), Cell::new("-")),
    };
    let key = match &indexer.key {
        KeyFrom::None => "-".to_string(),
        KeyFrom::Config => "config.yaml".to_string(),
        KeyFrom::Env(var) => format!("${var}"),
    };
    vec![Cell::styled(&indexer.name, Token::Accent), answers, Cell::new(key), categories, Cell::new(&indexer.url)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use torrential_core::indexer::IndexerCategory;

    fn category(id: u32, name: &str) -> IndexerCategory {
        IndexerCategory { id, name: name.into() }
    }

    #[test]
    fn the_list_shows_whether_each_answers_and_what_it_carries() {
        let list = IndexerList {
            indexers: vec![
                Indexer {
                    name: "thepiratebay".into(),
                    url: "http://127.0.0.1:9117/api/v2.0/indexers/thepiratebay/results/torznab/api".into(),
                    key: KeyFrom::Config,
                    check: Check::Answers {
                        categories: ["Console", "Movies", "Audio", "PC", "TV", "XXX"]
                            .iter()
                            .zip((1..).map(|n| n * 1000))
                            .map(|(name, id)| category(id, name))
                            .collect(),
                    },
                },
                Indexer {
                    name: "filelist".into(),
                    url: "http://127.0.0.1:9696/3/api".into(),
                    key: KeyFrom::Env("PROWLARR_KEY".into()),
                    check: Check::Fails { reason: "it refused the API key".into() },
                },
                Indexer {
                    name: "bitmagnet".into(),
                    url: "http://127.0.0.1:3333/torznab".into(),
                    key: KeyFrom::None,
                    check: Check::Answers { categories: vec![] },
                },
            ],
            added: vec!["thepiratebay".into()],
        };
        assert_eq!(
            list_doc(&list).plain(),
            "NAME\tANSWERS\tKEY\tCATEGORIES\tURL\n\
             thepiratebay\tyes\tconfig.yaml\tConsole, Movies, Audio, PC, +2\thttp://127.0.0.1:9117/api/v2.0/indexers/thepiratebay/results/torznab/api\n\
             filelist\tno\t$PROWLARR_KEY\t-\thttp://127.0.0.1:9696/3/api\n\
             bitmagnet\tyes\t-\t-\thttp://127.0.0.1:3333/torznab\n\
             # 3 indexers\n\
             # added thepiratebay\n\
             # filelist does not answer: it refused the API key\n"
        );
        assert_eq!(
            list_doc(&IndexerList::default()).plain(),
            "NAME\tANSWERS\tKEY\tCATEGORIES\tURL\n# 0 indexers\n# add one with torrential indexers add <address> --key <key>\n"
        );
    }
}
