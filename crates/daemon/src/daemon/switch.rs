//! How the engine follows its own updates: the news it shows the web app, the watcher that
//! installs releases, and the one sequence that stops this engine for the next.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use torrential_core::Error;

use super::ctx::Ctx;
use super::owner;
use super::playing::Closed;
use crate::config::Policy;
use crate::env::Env;
use crate::log::log;
use crate::paths::Paths;
use torrential_core::time::now;

use crate::update::release::{Put, Releases, check, installed, put_release};
use crate::update::{Audience, Checked, HOUR, UPDATED, Updated, Version, read_json, write_json};

/// What the web app says about versions.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum News {
    /// This engine started after an update installed `to` in place of `from`.
    Updated { from: String, to: String },
    /// Release `latest` is installed, and the login service starts it once nothing is playing.
    Waiting { latest: Version },
    /// Release `latest` is installed, and starts the next time the engine starts.
    Installed { latest: Version },
    /// A newer release that `torrential update` installs.
    Available { current: String, latest: Version },
}

impl News {
    pub fn of(policy: Policy, checked: &Checked, current: &str, updated_from: Option<&str>) -> Option<News> {
        if policy == Policy::Off {
            return None;
        }
        match checked.newer(current) {
            Some(latest) => Some(News::Available { current: current.to_string(), latest }),
            None => updated_from.map(|from| News::Updated { from: from.to_string(), to: current.to_string() }),
        }
    }

    pub fn line(&self) -> String {
        match self {
            Self::Updated { from, to } => Audience::Person.updated(from, to),
            Self::Waiting { latest } => format!("torrential {latest} is installed and starts when nothing is playing."),
            Self::Installed { latest } => {
                format!("torrential {latest} is installed and starts the next time the engine starts.")
            }
            Self::Available { current, latest } => Audience::Person.out(current, *latest),
        }
    }
}

/// The news the update watcher keeps for the web app.
#[derive(Clone, Default)]
pub struct Board(Arc<Mutex<Option<News>>>);

impl Board {
    pub fn post(&self, news: Option<News>) {
        *self.0.lock().expect("the board is never poisoned") = news;
    }

    pub fn line(&self) -> Option<String> {
        self.0.lock().expect("the board is never poisoned").as_ref().map(News::line)
    }
}

/// What the web app says the moment an engine starts, before the update watcher has looked at
/// the network: that this engine is the release just installed, unless updates are off.
pub fn opening(env: &Env, paths: &Paths) -> Option<News> {
    if matches!(crate::config::updates(paths), Ok(Policy::Off) | Err(_)) {
        return None;
    }
    let current = env.version();
    let from = replaced(paths, &current)?;
    Some(News::Updated { from, to: current })
}

/// The release this engine replaced, when an update installed the one it runs.
fn replaced(paths: &Paths, current: &str) -> Option<String> {
    read_json::<Updated>(&paths.state.join(UPDATED)).filter(|u| u.to == current).map(|u| u.from)
}

/// Looks for newer releases, posts what it finds on `board`, and under `updates: auto` installs
/// one. Returns the release the engine should switch to once one is installed.
pub async fn watch(env: Env, paths: Paths, board: &Board) -> Version {
    let current = env.version();
    let updated_from = replaced(&paths, &current);
    let Some(releases) = Releases::from_env(&env) else {
        board.post(updated_from.map(|from| News::Updated { from, to: current }));
        return std::future::pending().await;
    };
    loop {
        let now = now();
        let wait = match crate::config::updates(&paths) {
            Ok(Policy::Off) => {
                board.post(None);
                HOUR
            }
            Err(e) => {
                log(&format!("no update check, for the updates setting cannot be read: {}", e.message));
                board.post(None);
                HOUR
            }
            Ok(policy) => {
                if Checked::read(&paths).next_check() <= now
                    && let Err(e) = check(&releases, &paths, now).await
                {
                    log(&format!("update check failed: {}", e.message));
                }
                let checked = Checked::read(&paths);
                if policy == Policy::Auto
                    && checked.install_due(&current, now)
                    && let Some(latest) = checked.newer(&current)
                {
                    let put = async {
                        let (exe, target) = installed(&env)?;
                        let auto = || matches!(crate::config::updates(&paths), Ok(Policy::Auto));
                        put_release(&releases, &env, latest, &exe, target, auto).await
                    };
                    match put.await {
                        Ok(Put::Installed) => {
                            log(&format!("installed torrential {latest}"));
                            write_json(
                                &paths.state.join(UPDATED),
                                &Updated { from: current.clone(), to: latest.to_string() },
                            );
                            return latest;
                        }
                        Ok(Put::Current(installed)) => {
                            log(&format!("torrential {installed} is already installed"));
                            return installed;
                        }
                        Ok(Put::NotWanted) => {
                            log(&format!("updates is no longer auto, so torrential {latest} is not installed"));
                        }
                        Ok(Put::Refused(e)) => {
                            log(&format!("update failed: {}", e.message));
                            let mut checked = Checked::read(&paths);
                            checked.refused = Some(latest.to_string());
                            checked.write(&paths);
                        }
                        Err(e) => {
                            log(&format!("update failed: {}", e.message));
                            let mut checked = Checked::read(&paths);
                            checked.install_failed_at = Some(now);
                            checked.write(&paths);
                        }
                    }
                }
                let checked = Checked::read(&paths);
                board.post(News::of(policy, &checked, &current, updated_from.as_deref()));
                (checked.next_check() - now).clamp(60, HOUR)
            }
        };
        tokio::time::sleep(Duration::from_secs(wait as u64)).await;
    }
}

/// Installs updates as `watch` finds them, then switches to the release it installed.
pub async fn follow_updates(env: Env, paths: Paths, ctx: Arc<Ctx>, service: bool) {
    let to = watch(env, paths, &ctx.news).await;
    switch(&ctx, to, service).await
}

/// How long a switch to an installed update waits after the engine could not save every download.
const SWITCH_RETRY: Duration = Duration::from_secs(60);

/// Stops the engine for the login service to start release `to` once nothing plays and every
/// download is saved. An engine without a login service keeps running until it next starts.
async fn switch(ctx: &Ctx, to: Version, service: bool) {
    if !service {
        log(&format!("torrential {to} starts the next time the engine starts"));
        ctx.news.post(Some(News::Installed { latest: to }));
        return std::future::pending().await;
    }
    loop {
        if ctx.playing.any() {
            log(&format!("torrential {to} starts when nothing is playing"));
            ctx.news.post(Some(News::Waiting { latest: to }));
        }
        match stop_for_next(ctx, ctx.playing.close().await).await {
            Ok(()) => return,
            Err(e) => {
                log(&format!("torrential {to} waits: {}", e.message));
                tokio::time::sleep(SWITCH_RETRY).await;
            }
        }
    }
}

/// Stops the engine for the next one to start, once every download is saved. Playing stays shut
/// by `closed` until the engine exits; a stop that fails opens it again.
pub async fn stop_for_next(ctx: &Ctx, closed: Closed) -> Result<(), Error> {
    let stopped = ctx.owner.ask(|reply| owner::Command::Restart { reply }).await;
    if stopped.is_ok() {
        closed.keep();
    }
    stopped
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(text: &str) -> Version {
        Version::parse(text).unwrap()
    }

    #[test]
    fn the_web_app_hears_of_a_newer_release_or_the_update_that_started_the_engine() {
        let checked = |latest: &str| Checked { latest: Some(latest.into()), ..Checked::default() };
        let line = |policy, latest: &str, from| News::of(policy, &checked(latest), "0.5.0", from).map(|n| n.line());
        assert_eq!(
            line(Policy::Notify, "0.5.1", None).as_deref(),
            Some("torrential 0.5.1 is available (you have 0.5.0). Run torrential update to install it.")
        );
        assert_eq!(line(Policy::Auto, "0.5.0", Some("0.4.0")).as_deref(), Some("Updated torrential to 0.5.0."));
        assert_eq!(line(Policy::Auto, "0.5.0", None), None);
        assert_eq!(line(Policy::Off, "0.5.1", Some("0.4.0")), None);
        assert_eq!(
            News::Waiting { latest: version("0.5.1") }.line(),
            "torrential 0.5.1 is installed and starts when nothing is playing."
        );
        assert_eq!(
            News::Installed { latest: version("0.5.1") }.line(),
            "torrential 0.5.1 is installed and starts the next time the engine starts."
        );
    }

    #[test]
    fn a_refused_release_is_neither_installed_nor_announced_but_the_next_one_is() {
        let t = 1_790_000_000;
        let checked =
            |latest: &str| Checked { latest: Some(latest.into()), refused: Some("0.1.1".into()), ..Checked::default() };
        assert!(!checked("0.1.1").install_due("0.1.0", t));
        assert_eq!(News::of(Policy::Notify, &checked("0.1.1"), "0.1.0", None), None);
        assert!(checked("0.1.2").install_due("0.1.0", t));
        assert_eq!(
            News::of(Policy::Notify, &checked("0.1.2"), "0.1.0", None).map(|news| news.line()).as_deref(),
            Some("torrential 0.1.2 is available (you have 0.1.0). Run torrential update to install it.")
        );
    }
}
