use torrential_core::error::Details;
use torrential_core::{Error, SearchQuery, SourceOutcome, SourceReport};
use torrential_search::{ResultCache, Search, SearchConfig};

use crate::config::{self, Config};
use crate::output::views::{Check, CheckStatus, source_outcome};
use crate::paths::Paths;

const PROBE: &str = "linux";

pub fn checks(paths: &Paths, settings: Result<Config, Error>, network: bool) -> Vec<Check> {
    let check = |check: &str, status, detail: String| Check { check: check.into(), status, detail };
    let search = settings.and_then(|settings| {
        let config = SearchConfig { cache_dir: Some(paths.cache.join("search")), ..settings.search };
        Search::new(&config, ResultCache::new()).map(|search| (search, settings.file))
    });
    let (search, file) = match search {
        Ok(found) => found,
        Err(error) => {
            return vec![
                check("config", CheckStatus::Fail, error.message),
                check("sources", CheckStatus::Skipped, "the config has an error".into()),
            ];
        }
    };
    let detail = match file {
        Some(file) => file.display().to_string(),
        None => format!("no {}, so the defaults apply", config::path(paths).display()),
    };
    let mut checks = vec![check("config", CheckStatus::Ok, detail)];
    let enabled: Vec<String> = search.sources().into_iter().filter(|s| s.default_on).map(|s| s.name).collect();
    if !network {
        let detail = format!("{} enabled; torrential doctor --network asks each one", enabled.len());
        checks.push(check("sources", CheckStatus::Skipped, detail));
        return checks;
    }
    match ask(&search, enabled) {
        Ok(reports) => {
            let answered = reports.iter().filter(|r| r.outcome.is_ok()).count();
            let status = if answered == reports.len() { CheckStatus::Ok } else { CheckStatus::Fail };
            checks.push(check("sources", status, format!("{answered} of {} answered", reports.len())));
            checks.extend(reports.into_iter().map(|r| {
                let status = match r.outcome {
                    ref ok if ok.is_ok() => CheckStatus::Ok,
                    SourceOutcome::Irrelevant => CheckStatus::Warn,
                    _ => CheckStatus::Fail,
                };
                let detail = format!("{}, {} ms", source_outcome(&r.outcome), r.elapsed_ms);
                check(&format!("source {}", r.source), status, detail)
            }));
        }
        Err(error) => checks.push(check("sources", CheckStatus::Fail, error.message)),
    }
    checks
}

fn ask(search: &Search, sources: Vec<String>) -> Result<Vec<SourceReport>, Error> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| Error::internal(format!("cannot start the async runtime: {e}")))?;
    let query = SearchQuery { sources, fresh: true, ..SearchQuery::new(PROBE) };
    match runtime.block_on(search.search(&query)) {
        Ok(result) => Ok(result.sources),
        Err(Error { details: Some(Details::Sources(reports)), .. }) => Ok(reports),
        Err(error) => Err(error),
    }
}
