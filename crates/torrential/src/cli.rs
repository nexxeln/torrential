use std::path::PathBuf;
use std::time::Duration;

use clap::{Args, Parser, Subcommand, ValueEnum};
use clap_complete::engine::ArgValueCandidates;
use torrential_core::{Category, Playback, Screen, Sort};

use crate::completions::complete_download_ids;

#[derive(Parser, Debug)]
#[command(
    name = "torrential",
    version,
    about = "Search torrent indexes and download torrents",
    disable_help_subcommand = true,
    after_help = "Examples:\n  torrential search arch linux iso\n  torrential get 9b5212 --dir ~/isos\n  torrential wait 9b5212\n\nAgents: run torrential skill install to add the skill that explains torrential."
)]
pub struct Cli {
    /// Print JSON on stdout instead of text
    #[arg(long, global = true)]
    pub json: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Search the enabled sources
    #[command(
        after_help = "Examples:\n  torrential search ubuntu 24.04 desktop --limit 5\n  torrential search night of the living dead --source archive.org"
    )]
    Search(SearchArgs),
    /// List the files in a torrent, fetching its metadata if needed
    #[command(
        after_help = "Examples:\n  torrential files 6bc1af\n  torrential files 6bc1af --files '*.mp4' --files '*.srt'"
    )]
    Files(FilesArgs),
    /// Start a download, or change its file selection
    #[command(
        after_help = "Examples:\n  torrential get 9b5212\n  torrential get 'magnet:?xt=urn:btih:…' --dir ~/isos\n  torrential get 6bc1af --files '*.mp4' --dry-run"
    )]
    Get(GetArgs),
    /// List downloads, or show one in detail
    Status(StatusArgs),
    /// Follow downloads until they finish or the timeout passes
    #[command(
        after_help = "Examples:\n  torrential wait 9b5212\n  torrential wait 9b5212 --timeout 110m\n  torrential wait 9b5212 --until metadata"
    )]
    Wait(WaitArgs),
    /// Pause downloads, returning once writes stop
    Pause(RefsArgs),
    /// Resume downloads, or retry failed ones
    Resume(ResumeArgs),
    /// Stop tracking downloads and keep their files
    Rm(RefsArgs),
    /// Re-hash the data on disk and report damaged pieces
    Verify(VerifyArgs),
    /// Show what search ranks for on this machine, or set where you watch instead
    #[command(
        after_help = "Examples:\n  torrential setup\n  torrential setup --screen tv-4k\n  torrential setup --screen auto --max-size none\n  torrential setup --updates auto"
    )]
    Setup(SetupArgs),
    /// Diagnose sandbox, disk, state, engine, config, and sources
    Doctor(DoctorArgs),
    /// Open the terminal UI, which `torrential` alone also opens on a terminal
    Tui,
    /// Inspect or manage the engine process
    Daemon {
        #[command(subcommand)]
        command: DaemonCommand,
    },
    /// Print every command, flag, exit code, and error code as JSON
    Schema,
    /// Print shell completion setup
    Completions {
        #[arg(value_enum)]
        shell: Shell,
    },
    /// Install, check, or print the skill that teaches coding agents to use torrential
    Skill {
        #[command(subcommand)]
        command: crate::skill::SkillCommand,
    },
    /// Install the latest torrential, or check whether a newer one is out
    #[command(after_help = "Examples:\n  torrential update\n  torrential update --check")]
    Update(UpdateArgs),
}

#[derive(Args, Debug)]
pub struct UpdateArgs {
    /// Report the latest version without installing it
    #[arg(long)]
    pub check: bool,
}

#[derive(Args, Debug)]
pub struct SearchArgs {
    /// Words to search for
    #[arg(value_name = "WORDS", required = true, num_args = 1..)]
    pub words: Vec<String>,
    /// Search only this source. Repeat to add more
    #[arg(long, value_name = "NAME")]
    pub source: Vec<String>,
    /// Show only this kind of content
    #[arg(long, value_enum)]
    pub category: Option<CategoryArg>,
    /// Hide results with fewer reported seeders, and results with unknown seeders
    #[arg(long, value_name = "N")]
    pub min_seeds: Option<u32>,
    /// Hide results larger than this, for example 8GiB, and results with unknown size
    #[arg(long, value_name = "SIZE", value_parser = parse_size)]
    pub max_size: Option<u64>,
    /// Order of the results. Relevance comes before popularity
    #[arg(long, value_enum, default_value_t = SortArg::Relevance)]
    pub sort: SortArg,
    /// Number of results to show
    #[arg(long, value_name = "N", default_value_t = 10, value_parser = clap::value_parser!(u16).range(1..))]
    pub limit: u16,
    /// Stop waiting for slow sources after this long
    #[arg(long, value_name = "DURATION", default_value = "6s", value_parser = parse_duration)]
    pub timeout: Duration,
    /// Include adult results
    #[arg(long)]
    pub adult: bool,
    /// Skip the result cache
    #[arg(long)]
    pub fresh: bool,
}

#[derive(Args, Debug)]
pub struct SetupArgs {
    /// Where you watch, when it is not this machine's display. auto goes back to the display
    #[arg(long, value_enum)]
    pub screen: Option<ScreenArg>,
    /// The largest download get starts without its own --max-size, for example 8GB, or none
    #[arg(long, value_name = "SIZE", value_parser = parse_limit)]
    pub max_size: Option<Limit>,
    /// What your player handles, when it is not this machine's. auto goes back to the display
    #[arg(long, value_enum)]
    pub playback: Option<PlaybackArg>,
    /// Whether torrential says when a newer version is out, installs it in the background, or never looks
    #[arg(long, value_enum)]
    pub updates: Option<crate::update::Policy>,
}

pub const SETUP_FLAGS: &str = "torrential setup --screen phone|laptop|tv|tv-4k|auto --max-size 8GB|none --playback \
                               hevc-hdr|hevc|h264|auto --updates notify|auto|off";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Limit(pub Option<u64>);

#[derive(Args, Debug)]
pub struct DoctorArgs {
    /// Also search each enabled source once, and report how it answered and how long it took
    #[arg(long)]
    pub network: bool,
}

#[derive(Args, Debug)]
pub struct FilesArgs {
    /// An ID prefix, infohash, magnet link, .torrent file, or .torrent URL
    #[arg(value_name = "REF", add = ArgValueCandidates::new(complete_download_ids))]
    pub reference: String,
    /// List only files matching this glob or exact path, as get --files would pick them. `*` crosses folders and case is ignored. Repeat to add more
    #[arg(long, value_name = "GLOB")]
    pub files: Vec<String>,
}

#[derive(Args, Debug)]
pub struct GetArgs {
    /// An ID prefix, infohash, magnet link, .torrent file, or .torrent URL
    #[arg(value_name = "REF", add = ArgValueCandidates::new(complete_download_ids))]
    pub reference: String,
    /// Download only files matching this glob or exact path. `*` crosses folders and case is ignored. Repeat to add more
    #[arg(long, value_name = "GLOB")]
    pub files: Vec<String>,
    /// The folder the download goes into
    #[arg(long, value_name = "DIR")]
    pub dir: Option<PathBuf>,
    /// The largest download allowed, for example 120GiB. Without it the cap is the max size from torrential setup, or 50 GiB
    #[arg(long, value_name = "SIZE", value_parser = parse_size)]
    pub max_size: Option<u64>,
    /// Print the selected size, destination, free space, and conflicts without downloading
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Args, Debug)]
pub struct StatusArgs {
    /// Download IDs from torrential status. None lists every download
    #[arg(value_name = "ID", add = ArgValueCandidates::new(complete_download_ids))]
    pub references: Vec<String>,
}

#[derive(Args, Debug)]
pub struct WaitArgs {
    /// Download IDs from torrential status
    #[arg(value_name = "ID", required = true, num_args = 1.., add = ArgValueCandidates::new(complete_download_ids))]
    pub references: Vec<String>,
    /// Stop when the download is done, or as soon as its file list is known
    #[arg(long, value_enum, default_value_t = Until::Done)]
    pub until: Until,
    /// Give up after this long. Defaults to 100s, or no limit when a person watches in a terminal
    #[arg(long, value_name = "DURATION", value_parser = parse_duration)]
    pub timeout: Option<Duration>,
}

#[derive(Args, Debug)]
pub struct RefsArgs {
    /// Download IDs from torrential status
    #[arg(value_name = "ID", required = true, num_args = 1.., add = ArgValueCandidates::new(complete_download_ids))]
    pub references: Vec<String>,
}

#[derive(Args, Debug)]
pub struct ResumeArgs {
    /// Download IDs from torrential status
    #[arg(value_name = "ID", required_unless_present = "all", add = ArgValueCandidates::new(complete_download_ids))]
    pub references: Vec<String>,
    /// Resume every paused, interrupted, or failed download
    #[arg(long, conflicts_with = "references")]
    pub all: bool,
}

#[derive(Args, Debug)]
pub struct VerifyArgs {
    /// A download ID from torrential status
    #[arg(value_name = "ID", add = ArgValueCandidates::new(complete_download_ids))]
    pub reference: String,
    /// Download damaged pieces again
    #[arg(long)]
    pub repair: bool,
}

#[derive(Subcommand, Debug)]
pub enum DaemonCommand {
    /// Show whether the engine is running
    Status,
    /// Ask the engine to save its state and exit
    Stop,
    /// Start the engine at login with launchd or systemd
    Install,
    /// Remove the login service
    Uninstall,
    /// Run the engine in the foreground
    #[command(hide = true)]
    Run {
        /// Started by launchd or systemd: resume downloads at startup and never exit when idle
        #[arg(long)]
        service: bool,
    },
    /// The short-lived middle process of the detached start
    #[command(hide = true)]
    Launch,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, ValueEnum)]
pub enum Until {
    Done,
    Metadata,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, ValueEnum)]
pub enum Shell {
    Bash,
    Zsh,
    Fish,
    Elvish,
    Powershell,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, ValueEnum)]
pub enum CategoryArg {
    Video,
    Audio,
    Software,
    Book,
    Game,
    Dataset,
    Other,
}

impl From<CategoryArg> for Category {
    fn from(arg: CategoryArg) -> Self {
        match arg {
            CategoryArg::Video => Category::Video,
            CategoryArg::Audio => Category::Audio,
            CategoryArg::Software => Category::Software,
            CategoryArg::Book => Category::Book,
            CategoryArg::Game => Category::Game,
            CategoryArg::Dataset => Category::Dataset,
            CategoryArg::Other => Category::Other,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, ValueEnum)]
pub enum ScreenArg {
    /// A phone or tablet: 720p
    Phone,
    /// A laptop or monitor: 1080p
    Laptop,
    /// A 1080p TV
    Tv,
    /// A 4K TV: 2160p
    #[value(name = "tv-4k")]
    Tv4k,
    /// This machine's largest display
    Auto,
}

impl ScreenArg {
    pub fn screen(self) -> Option<Screen> {
        match self {
            Self::Phone => Some(Screen::Phone),
            Self::Laptop => Some(Screen::Laptop),
            Self::Tv => Some(Screen::Tv),
            Self::Tv4k => Some(Screen::Tv4k),
            Self::Auto => None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, ValueEnum)]
pub enum PlaybackArg {
    /// HEVC (x265) and HDR
    #[value(name = "hevc-hdr")]
    HevcHdr,
    /// HEVC but not HDR
    Hevc,
    /// H.264 (x264) only, which every player handles
    H264,
    /// HEVC, and HDR when this machine's display shows it
    Auto,
}

impl PlaybackArg {
    pub fn playback(self) -> Option<Playback> {
        match self {
            Self::HevcHdr => Some(Playback::HevcHdr),
            Self::Hevc => Some(Playback::Hevc),
            Self::H264 => Some(Playback::H264),
            Self::Auto => None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, ValueEnum)]
pub enum SortArg {
    Relevance,
    Seeds,
    Size,
    Age,
}

impl From<SortArg> for Sort {
    fn from(arg: SortArg) -> Self {
        match arg {
            SortArg::Relevance => Sort::Relevance,
            SortArg::Seeds => Sort::Seeds,
            SortArg::Size => Sort::Size,
            SortArg::Age => Sort::Age,
        }
    }
}

fn parse_size(s: &str) -> Result<u64, String> {
    torrential_core::units::parse_size(s).ok_or_else(|| format!("{s:?} is not a size; use a form like 8GiB or 700MB"))
}

fn parse_limit(s: &str) -> Result<Limit, String> {
    if s.trim().eq_ignore_ascii_case("none") {
        return Ok(Limit(None));
    }
    torrential_core::units::parse_size(s)
        .map(|size| Limit(Some(size)))
        .ok_or_else(|| format!("{s:?} is not a size; use a form like 8GB, or none"))
}

fn parse_duration(s: &str) -> Result<Duration, String> {
    torrential_core::units::parse_duration(s)
        .ok_or_else(|| format!("{s:?} is not a duration; use a form like 100s, 5m, or 1h30m"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn definition_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn get_parses_repeated_globs_and_sizes() {
        let cli = Cli::try_parse_from([
            "torrential",
            "get",
            "6bc1af",
            "--files",
            "*.mp4",
            "--files",
            "*.srt",
            "--max-size",
            "60GiB",
        ])
        .unwrap();
        let Some(Command::Get(args)) = cli.command else { panic!("expected get") };
        assert_eq!(args.files, ["*.mp4", "*.srt"]);
        assert_eq!(args.max_size, Some(60 * 1024 * 1024 * 1024));
    }

    #[test]
    fn json_is_accepted_after_the_subcommand() {
        let cli = Cli::try_parse_from(["torrential", "status", "--json"]).unwrap();
        assert!(cli.json);
    }

    #[test]
    fn setup_flags_name_the_values_the_config_stores_and_auto_stores_none() {
        let flag = |value: Option<clap::builder::PossibleValue>| value.unwrap().get_name().to_string();
        let screens = ScreenArg::value_variants()
            .iter()
            .map(|v| (flag(v.to_possible_value()), v.screen().map_or("auto", Screen::as_str).to_string()));
        let players = PlaybackArg::value_variants()
            .iter()
            .map(|v| (flag(v.to_possible_value()), v.playback().map_or("auto", Playback::as_str).to_string()));
        for (flag, stored) in screens.chain(players) {
            assert_eq!(flag, stored);
        }
    }

    #[test]
    fn every_value_the_setup_flags_line_lists_parses() {
        let words: Vec<&str> = SETUP_FLAGS.split(' ').collect();
        for pair in words[2..].chunks(2) {
            for value in pair[1].split('|') {
                let parsed = Cli::try_parse_from(["torrential", "setup", pair[0], value]);
                assert!(parsed.is_ok(), "{} {value}: {:?}", pair[0], parsed.err());
            }
        }
    }

    #[test]
    fn setup_takes_a_size_or_none() {
        let max_size = |value: &str| match Cli::try_parse_from(["torrential", "setup", "--max-size", value]) {
            Ok(Cli { command: Some(Command::Setup(args)), .. }) => Ok(args.max_size),
            Ok(_) => panic!("expected setup"),
            Err(e) => Err(e.kind()),
        };
        assert_eq!(max_size("8GB"), Ok(Some(Limit(Some(8_000_000_000)))));
        assert_eq!(max_size("none"), Ok(Some(Limit(None))));
        assert_eq!(max_size("big"), Err(clap::error::ErrorKind::ValueValidation));
    }

    #[test]
    fn bad_durations_name_the_expected_form() {
        let err = Cli::try_parse_from(["torrential", "wait", "9b5212", "--timeout", "soon"]).unwrap_err();
        assert!(err.to_string().contains("use a form like 100s"), "{err}");
    }
}
