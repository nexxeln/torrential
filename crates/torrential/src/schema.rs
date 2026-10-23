use clap::{Arg, ArgAction, Command};
use serde::Serialize;
use torrential_core::ErrorCode;
use torrential_core::error::EXIT_PENDING;

#[derive(Serialize)]
pub struct Schema {
    #[serde(rename = "type")]
    kind: &'static str,
    version: String,
    commands: Vec<CommandSchema>,
    global_flags: Vec<FlagSchema>,
    exit_codes: Vec<ExitSchema>,
    errors: Vec<ErrorSchema>,
    fields: &'static [FieldSchema],
}

#[derive(Serialize)]
struct CommandSchema {
    name: String,
    about: String,
    arguments: Vec<ArgumentSchema>,
    flags: Vec<FlagSchema>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    subcommands: Vec<CommandSchema>,
}

#[derive(Serialize)]
struct ArgumentSchema {
    name: String,
    help: String,
    required: bool,
    repeatable: bool,
}

#[derive(Serialize)]
struct FlagSchema {
    name: String,
    help: String,
    takes_value: bool,
    repeatable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    value_name: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    values: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    default: Option<String>,
}

#[derive(Serialize)]
struct ExitSchema {
    code: u8,
    meaning: &'static str,
}

#[derive(Serialize)]
struct ErrorSchema {
    code: &'static str,
    exit: u8,
    retryable: bool,
}

#[derive(Serialize)]
struct FieldSchema {
    name: &'static str,
    help: &'static str,
}

const FIELDS: [FieldSchema; 3] = [
    FieldSchema {
        name: "progress.piece_map",
        help: "Where a download's data has arrived, in status, wait, and the TUI's watch stream: 64 characters, one \
               per equal run of the torrent's pieces in order. 0 to 8 are eighths of the run's selected pieces that \
               passed their hash check, and - is a run with no selected piece. Absent before the engine first samples \
               it, which needs the file list, and once the download is done",
    },
    FieldSchema {
        name: "result.releases[].quality",
        help: "What the release name says about the picture: resolution is 2160p, 1080p, 720p, or sd, codec is h264, \
               hevc, or av1, and each is null when the name does not say. hdr is true for an HDR10, HDR10+, or Dolby \
               Vision tag",
    },
    FieldSchema {
        name: "result.query.profile",
        help: "What the results were ranked for. screen and playback each carry a value and a source, detected or \
               override, and are null when nothing is known. max_size comes from the preferences that torrential \
               setup saves. room, arch, and language are detected, and room is the bytes a download may take in the \
               download folder. detected is null until the engine finishes detecting the machine after it starts. \
               Within equal relevance, results that fit max_size, the room, and the player come first, then \
               official and lawful sources, then the resolution closest to the screen, then the machine's CPU \
               architecture, then releases in the system language",
    },
];

pub fn build(cli: &Command, version: String) -> Schema {
    let global_flags = cli.get_arguments().filter(|a| a.is_global_set()).map(flag).collect();
    Schema {
        kind: "schema",
        version,
        commands: visible(cli).map(command).collect(),
        global_flags,
        exit_codes: exit_codes(),
        errors: ErrorCode::ALL
            .iter()
            .map(|c| ErrorSchema { code: c.as_str(), exit: c.exit_code(), retryable: c.retryable() })
            .collect(),
        fields: &FIELDS,
    }
}

fn visible(cmd: &Command) -> impl Iterator<Item = &Command> {
    cmd.get_subcommands().filter(|c| !c.is_hide_set())
}

fn command(cmd: &Command) -> CommandSchema {
    let own = cmd.get_arguments().filter(|a| !a.is_global_set() && !a.is_hide_set() && a.get_id() != "help");
    let (positional, options): (Vec<&Arg>, Vec<&Arg>) = own.partition(|a| a.is_positional());
    CommandSchema {
        name: cmd.get_name().to_string(),
        about: cmd.get_about().map(|s| s.to_string()).unwrap_or_default(),
        arguments: positional
            .into_iter()
            .map(|a| ArgumentSchema {
                name: a
                    .get_value_names()
                    .and_then(|v| v.first())
                    .map_or_else(|| a.get_id().to_string(), |v| v.to_string()),
                help: help(a),
                required: a.is_required_set(),
                repeatable: repeatable(a),
            })
            .collect(),
        flags: options.into_iter().map(flag).collect(),
        subcommands: visible(cmd).map(command).collect(),
    }
}

fn flag(a: &Arg) -> FlagSchema {
    let takes_value = a.get_action().takes_values();
    FlagSchema {
        name: format!("--{}", a.get_long().unwrap_or(a.get_id().as_str())),
        help: help(a),
        takes_value,
        repeatable: repeatable(a),
        value_name: takes_value.then(|| a.get_value_names().and_then(|v| v.first()).map(|v| v.to_string())).flatten(),
        values: if takes_value {
            a.get_possible_values().iter().filter(|v| !v.is_hide_set()).map(|v| v.get_name().to_string()).collect()
        } else {
            Vec::new()
        },
        default: a.get_default_values().first().map(|v| v.to_string_lossy().into_owned()),
    }
}

fn help(a: &Arg) -> String {
    a.get_help().map(|s| s.to_string()).unwrap_or_default()
}

fn repeatable(a: &Arg) -> bool {
    matches!(a.get_action(), ArgAction::Append | ArgAction::Count)
        || a.get_num_args().is_some_and(|n| n.max_values() > 1)
}

fn exit_codes() -> Vec<ExitSchema> {
    let mut codes: Vec<u8> = ErrorCode::ALL.iter().map(|c| c.exit_code()).collect();
    codes.extend([0, EXIT_PENDING]);
    codes.sort_unstable();
    codes.dedup();
    codes.into_iter().map(|code| ExitSchema { code, meaning: exit_meaning(code) }).collect()
}

fn exit_meaning(code: u8) -> &'static str {
    match code {
        0 => "success, including an empty search",
        1 => "internal error",
        2 => "usage error",
        3 => "the reference matched nothing, matched several, or is malformed",
        4 => "conflict with an existing download or file",
        5 => "network or search provider failure",
        6 => "environment, sandbox, engine, or state failure",
        7 => "disk, size, selection, or verification failure, or a download that stopped with an error",
        EXIT_PENDING => "still pending: the download is running or stopped without finishing",
        130 => "interrupted",
        _ => "unassigned",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    fn schema() -> serde_json::Value {
        serde_json::to_value(build(&crate::cli::Cli::command(), "0.1.0".into())).unwrap()
    }

    #[test]
    fn lists_every_visible_command_and_no_hidden_ones() {
        let names: Vec<String> =
            schema()["commands"].as_array().unwrap().iter().map(|c| c["name"].as_str().unwrap().to_string()).collect();
        assert_eq!(
            names,
            [
                "search",
                "files",
                "get",
                "status",
                "wait",
                "pause",
                "resume",
                "rm",
                "verify",
                "setup",
                "doctor",
                "tui",
                "engine",
                "schema",
                "completions",
                "skill",
                "update"
            ]
        );
        let daemon = &schema()["commands"][12]["subcommands"];
        let subs: Vec<&str> = daemon.as_array().unwrap().iter().map(|c| c["name"].as_str().unwrap()).collect();
        assert_eq!(subs, ["status", "stop", "install", "uninstall"]);
    }

    #[test]
    fn describes_flags_with_their_values() {
        let s = schema();
        let get = &s["commands"][2];
        let files = get["flags"].as_array().unwrap().iter().find(|f| f["name"] == "--files").unwrap();
        assert_eq!(files["repeatable"], true);
        assert_eq!(files["value_name"], "GLOB");
        let wait = &s["commands"][4];
        let until = wait["flags"].as_array().unwrap().iter().find(|f| f["name"] == "--until").unwrap();
        assert_eq!(until["values"], serde_json::json!(["done", "metadata"]));
        assert_eq!(until["default"], "done");
        assert_eq!(s["global_flags"][0]["name"], "--json");
    }

    #[test]
    fn exit_codes_cover_every_error_code() {
        let s = schema();
        let exits: Vec<u64> = s["exit_codes"].as_array().unwrap().iter().map(|e| e["code"].as_u64().unwrap()).collect();
        assert_eq!(exits, [0, 1, 2, 3, 4, 5, 6, 7, 8, 130]);
        assert_eq!(s["errors"].as_array().unwrap().len(), ErrorCode::ALL.len());
        assert_eq!(s["errors"][5], serde_json::json!({"code": "ambiguous_ref", "exit": 3, "retryable": false}));
    }
}
