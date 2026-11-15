use serde::Serialize;
use torrential_core::Error;
use torrential_core::time::now;

use crate::cli::UpdateArgs;
use crate::config::Policy;
use crate::env::Env;
use crate::fsutil::write_atomic;
use crate::output::{self, Body, Cell, Doc, DocKind, Mode, Typed};
use crate::paths::Paths;
use crate::update::release::{self, Outcome, Releases};
use crate::update::{Audience, Checked, UPDATED, Updated, Version, due, read_json};

#[derive(Serialize)]
struct CheckReport<'a> {
    current: &'a str,
    latest: String,
    update_available: bool,
    policy: Policy,
}

#[derive(Serialize)]
struct UpdateReport<'a> {
    current: &'a str,
    latest: String,
    result: Installed,
}

torrential_core::words! {
    /// What `update` did.
    enum Installed {
        Updated = "updated",
        UpToDate = "up_to_date",
    }
}

pub fn run(args: UpdateArgs, mode: Mode, env: &Env) -> Result<u8, Error> {
    let paths = Paths::from_process(env)?;
    let releases = Releases::required(env)?;
    let runtime = release::runtime()?;
    let current = env.version();
    if args.check {
        let policy = crate::config::updates(&paths)?;
        let latest = runtime.block_on(release::check(&releases, &paths, now()))?;
        let update_available = latest.newer_than(&current);
        let fields = vec![
            ("current", Cell::new(&current)),
            ("latest", Cell::new(latest.to_string())),
            ("policy", Cell::new(policy.as_str())),
        ];
        let doc = Doc {
            body: Body::Fields { title: Vec::new(), fields },
            summary: Vec::new(),
            next: update_available.then(|| "torrential update".to_string()),
        };
        let body = CheckReport { current: &current, latest: latest.to_string(), update_available, policy };
        return Ok(super::emit(mode, &doc, &Typed { kind: DocKind::UpdateCheck, body }));
    }
    let (latest, result) = match runtime.block_on(release::install(&releases, &paths, env, now()))? {
        Outcome::Updated(latest) => (latest, Installed::Updated),
        Outcome::Latest(latest) => (latest, Installed::UpToDate),
    };
    match mode {
        Mode::Json => {
            let body = UpdateReport { current: &current, latest: latest.to_string(), result };
            print!("{}", output::json_document(&Typed { kind: DocKind::Update, body }, None));
        }
        _ if result == Installed::Updated => println!("Updated torrential {current} → {latest}."),
        _ => println!("torrential {current} is up to date."),
    }
    Ok(0)
}

pub fn available(paths: &Paths, env: &Env) -> Option<Version> {
    if crate::config::updates(paths).ok()? == Policy::Off {
        return None;
    }
    Checked::read(paths).newer(&env.version())
}

pub fn notice(paths: &Paths, env: &Env, audience: Audience, now: i64) -> Option<String> {
    let updated = paths.state.join(UPDATED);
    if let Some(Updated { from, to }) = read_json(&updated)
        && std::fs::remove_file(&updated).is_ok()
    {
        return Some(audience.updated(&from, &to));
    }
    let latest = available(paths, env)?;
    let file = audience.notice_file(paths);
    let last = std::fs::read_to_string(&file).ok().and_then(|text| text.trim().parse().ok());
    if !due(last, now) {
        return None;
    }
    let _ = write_atomic(&file, now.to_string().as_bytes());
    Some(audience.out(&env.version(), latest))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::update::{DAY, HOUR};

    #[test]
    fn each_audience_hears_once_a_day_and_off_silences_both() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().display().to_string();
        let env = Env::from_pairs([("HOME", home.as_str()), ("TORRENTIAL_HOME", home.as_str())]);
        let paths = Paths::resolve(&env, 501, None).unwrap();
        let t = 1_790_000_000;
        Checked { latest: Some("9.0.0".into()), checked_at: Some(t), ..Checked::default() }.write(&paths);
        let heard = |audience, now| notice(&paths, &env, audience, now).is_some();

        assert_eq!(
            [heard(Audience::Person, t), heard(Audience::Person, t + HOUR), heard(Audience::Agent, t + HOUR)],
            [true, false, true]
        );
        assert!(heard(Audience::Person, t + DAY));
        std::fs::create_dir_all(&paths.config).unwrap();
        std::fs::write(paths.config.join("config.yaml"), "updates: off\n").unwrap();
        assert!(!heard(Audience::Person, t + 3 * DAY));
    }
}
