use std::path::Path;

use serde::Serialize;
use torrential_core::Machine;
use torrential_core::units::format_limit;
use torrential_core::units::size_text;
use torrential_core::{Error, Preferences, Profile};

use crate::cli::{Limit, PlaybackArg, ScreenArg, SetupArgs};
use crate::output::theme::Token;
use crate::output::{Body, Cell, Doc};
use crate::output::{DocKind, Mode, Typed};
use torrential_daemon::config;
use torrential_daemon::config::Policy;
use torrential_daemon::env::Env;
use torrential_daemon::fsutil::Writes;
use torrential_daemon::paths::Paths;

torrential_core::words! {
    /// What `setup` did with the settings it was given.
    pub enum Setup {
        Shown = "shown",
        Saved = "saved",
        Unchanged = "unchanged",
    }
}

#[derive(Serialize)]
struct Report<'a> {
    result: Setup,
    profile: &'a Profile,
    updates: Policy,
    file: String,
}

pub fn run(args: SetupArgs, mode: Mode, env: &Env) -> Result<u8, Error> {
    let paths = Paths::from_process(env)?;
    let current = config::preferences(&paths)?;
    let wanted = Preferences {
        screen: args.screen.map_or(current.screen, ScreenArg::screen),
        max_size: args.max_size.map_or(current.max_size, |Limit(max)| max),
        playback: args.playback.map_or(current.playback, PlaybackArg::playback),
    };
    let updates = match args.updates {
        Some(updates) => updates,
        None => config::updates(&paths)?,
    };
    let preferences_flagged = args.screen.is_some() || args.max_size.is_some() || args.playback.is_some();
    let writes = Writes::new(env, command(&wanted, updates));
    let mut result = Setup::Shown;
    if preferences_flagged || args.updates.is_some() {
        let mut changed = false;
        if preferences_flagged {
            changed |= config::save_preferences(&paths, &wanted, &writes)?.1;
        }
        if args.updates.is_some() {
            changed |= config::save_updates(&paths, updates, &writes)?.1;
        }
        result = if changed { Setup::Saved } else { Setup::Unchanged };
    }
    let file = config::file(&paths)?;
    let profile =
        Profile { detected: Some(torrential_daemon::machine::detect(env)?), preferences: wanted, ..Profile::default() };
    let doc = setup(&profile, updates, &file, result);
    let report = Report { result, profile: &profile, updates, file: file.display().to_string() };
    Ok(super::emit(mode, &doc, &Typed { kind: DocKind::Setup, body: report }))
}

fn command(preferences: &Preferences, updates: Policy) -> String {
    format!(
        "torrential setup --screen {} --max-size {} --playback {} --updates {}",
        preferences.screen.map_or("auto", |s| s.as_str()),
        preferences.max_size.map_or_else(|| "none".to_string(), size_text),
        preferences.playback.map_or("auto", |p| p.as_str()),
        updates.as_str(),
    )
}

pub fn setup(profile: &Profile, updates: Policy, file: &Path, result: Setup) -> Doc {
    let detected = profile.detected.as_ref().map_or_else(|| "nothing".to_string(), machine);
    let fields = vec![
        ("detected", Cell::new(detected)),
        ("updates", Cell::new(updates.as_str())),
        ("file", Cell::new(file.display().to_string())),
    ];
    let title = match result {
        Setup::Shown => Vec::new(),
        word => vec![Cell::styled(word.as_str(), Token::Mint)],
    };
    let overrides = profile.overrides().unwrap_or_else(|| "none".to_string());
    let mut summary = vec![format!("ranking for {} · overrides: {overrides}", profile.describe())];
    if let Some(max) = profile.preferences.max_size {
        summary.push(format!("get rejects downloads over {} unless --max-size is higher", format_limit(max)));
    }
    let next = if result == Setup::Shown { crate::cli::SETUP_FLAGS } else { "torrential search <words>" };
    Doc { body: Body::Fields { title, fields }, summary, next: Some(next.to_string()) }
}

fn machine(machine: &Machine) -> String {
    let display = match machine.display {
        Some(d) => format!("{}x{} display{}", d.width, d.height, if d.hdr { " with HDR" } else { "" }),
        None => "no display".to_string(),
    };
    let arch = machine.arch.map(|a| a.as_str().to_string());
    let language = machine.language.as_ref().map(|l| l.name().to_string());
    [Some(display), arch, language].into_iter().flatten().collect::<Vec<_>>().join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use torrential_core::{Playback, Screen};

    #[test]
    fn the_rerun_command_names_each_preference_or_what_clears_it() {
        let laptop =
            Preferences { screen: Some(Screen::Laptop), max_size: Some(2_000_000_000), playback: Some(Playback::H264) };
        assert_eq!(
            command(&laptop, Policy::Auto),
            "torrential setup --screen laptop --max-size 2GB --playback h264 --updates auto"
        );
        assert_eq!(
            command(&Preferences::default(), Policy::Notify),
            "torrential setup --screen auto --max-size none --playback auto --updates notify"
        );
    }
}
