use serde::Serialize;
use torrential_core::Error;

use crate::cli::UpdateArgs;
use crate::daemon::owner::now;
use crate::env::Env;
use crate::output::{self, Body, Cell, Doc, Mode, Typed};
use crate::paths::Paths;
use crate::update::{self, Outcome, Policy, Releases};

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
    result: &'static str,
}

pub fn run(args: UpdateArgs, mode: Mode, env: &Env) -> Result<u8, Error> {
    let paths = Paths::from_process(env)?;
    let releases = Releases::required(env)?;
    let runtime = update::runtime()?;
    let current = env.version();
    if args.check {
        let policy = crate::config::updates(&paths)?;
        let latest = runtime.block_on(update::check(&releases, &paths, now()))?;
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
        return Ok(super::emit(mode, &doc, &Typed { kind: "update_check", body }));
    }
    let (latest, result) = match runtime.block_on(update::install(&releases, &paths, env, now()))? {
        Outcome::Updated(latest) => (latest, "updated"),
        Outcome::Latest(latest) => (latest, "up_to_date"),
    };
    match mode {
        Mode::Json => {
            let body = UpdateReport { current: &current, latest: latest.to_string(), result };
            print!("{}", output::json_document(&Typed { kind: "update", body }, None));
        }
        _ if result == "updated" => println!("Updated torrential {current} → {latest}."),
        _ => println!("torrential {current} is up to date."),
    }
    Ok(0)
}
