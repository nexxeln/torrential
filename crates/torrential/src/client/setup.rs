use serde::Serialize;
use torrential_core::units::size_text;
use torrential_core::{Error, Preferences, Profile};

use crate::cli::{Limit, PlaybackArg, ScreenArg, SetupArgs};
use crate::config;
use crate::env::Env;
use crate::output::{Mode, Typed, views};
use crate::paths::Paths;
use crate::skill::Writes;
use crate::update::Policy;

#[derive(Serialize)]
struct Report<'a> {
    result: &'static str,
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
    let mut result = "shown";
    if preferences_flagged || args.updates.is_some() {
        let mut changed = false;
        if preferences_flagged {
            changed |= config::save_preferences(&paths, &wanted, &writes)?.1;
        }
        if args.updates.is_some() {
            changed |= config::save_updates(&paths, updates, &writes)?.1;
        }
        result = if changed { "saved" } else { "unchanged" };
    }
    let file = config::file(&paths)?;
    let profile = Profile { detected: Some(crate::machine::detect(env)?), preferences: wanted, ..Profile::default() };
    let doc = views::setup(&profile, updates, &file, result);
    let report = Report { result, profile: &profile, updates, file: file.display().to_string() };
    Ok(super::emit(mode, &doc, &Typed { kind: "setup", body: report }))
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
