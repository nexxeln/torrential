use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use torrential_core::units::{parse_duration, parse_size};
use torrential_core::{Error, ErrorCode, TorrentId};

pub mod hooks;

#[derive(Clone, Debug, Default)]
pub struct Env {
    vars: HashMap<String, String>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Agent {
    ClaudeCode,
    Codex,
    Gemini,
    Cursor,
    Other,
}

const AGENT_VARS: [(&str, Agent); 7] = [
    ("CLAUDECODE", Agent::ClaudeCode),
    ("CLAUDE_CODE_CHILD_SESSION", Agent::ClaudeCode),
    ("CODEX_THREAD_ID", Agent::Codex),
    ("CODEX_CI", Agent::Codex),
    ("GEMINI_CLI", Agent::Gemini),
    ("CURSOR_AGENT", Agent::Cursor),
    ("AI_AGENT", Agent::Other),
];

impl Env {
    pub fn from_process() -> Self {
        Self { vars: std::env::vars().collect() }
    }

    #[cfg(test)]
    pub fn from_pairs<'a>(pairs: impl IntoIterator<Item = (&'a str, &'a str)>) -> Self {
        Self { vars: pairs.into_iter().map(|(k, v)| (k.to_string(), v.to_string())).collect() }
    }

    /// This environment with `key` set to `value`.
    pub fn with(mut self, key: &str, value: &str) -> Self {
        self.vars.insert(key.to_string(), value.to_string());
        self
    }

    pub fn var(&self, key: &str) -> Option<&str> {
        self.vars.get(key).map(String::as_str).filter(|v| !v.is_empty())
    }

    pub fn sandbox_harness(&self) -> Option<Agent> {
        if self.var("CODEX_SANDBOX").is_some() || self.var("CODEX_SANDBOX_NETWORK_DISABLED").is_some() {
            return Some(Agent::Codex);
        }
        self.agent()
    }

    pub fn agent(&self) -> Option<Agent> {
        let (_, agent) = AGENT_VARS.iter().find(|(key, _)| self.var(key).is_some())?;
        if *agent == Agent::Other && self.var("AI_AGENT").is_some_and(|v| v.starts_with("claude-code")) {
            return Some(Agent::ClaudeCode);
        }
        Some(*agent)
    }

    pub fn version(&self) -> String {
        self.test().and_then(|test| test.var(hooks::VERSION)).unwrap_or(env!("CARGO_PKG_VERSION")).to_string()
    }

    /// The settings a test gave this process, or `None` when no test started it. A test runs the
    /// fake engine or the loopback network; any other process does nothing for tests.
    pub fn test(&self) -> Option<TestHooks<'_>> {
        let test = self.var(hooks::ENGINE) == Some("fake") || self.var(hooks::NETWORK) == Some("loopback");
        test.then_some(TestHooks(self))
    }
}

/// The `TORRENTIAL_TEST_*` settings of a process a test started, each read and parsed here.
#[derive(Clone, Copy)]
pub struct TestHooks<'a>(&'a Env);

/// The longest a test may hold the engine between two steps.
const MAX_PAUSE: Duration = Duration::from_secs(60);

impl<'a> TestHooks<'a> {
    pub fn var(&self, name: &str) -> Option<&'a str> {
        self.0.var(name)
    }

    pub fn duration(&self, name: &str) -> Option<Duration> {
        self.var(name).and_then(parse_duration)
    }

    pub fn path(&self, name: &str) -> Option<&'a Path> {
        self.var(name).map(Path::new)
    }

    pub fn fake_engine(&self) -> bool {
        self.var(hooks::ENGINE) == Some("fake")
    }

    /// Whether the fake engine uses the real source named by `hook`, `SUBTITLES` or `CATALOG`.
    pub fn live(&self, hook: &str) -> bool {
        self.var(hook) == Some("live")
    }

    /// The peers of the loopback network, or `None` on the public network.
    pub fn loopback_peers(&self) -> Result<Option<Vec<SocketAddr>>, Error> {
        if self.var(hooks::NETWORK) != Some("loopback") {
            return Ok(None);
        }
        let Some(list) = self.var(hooks::PEERS) else { return Ok(Some(Vec::new())) };
        let peer = |peer: &str| {
            peer.trim().parse().map_err(|_| {
                Error::new(ErrorCode::Usage, format!("{} has {peer:?}, which is not an address", hooks::PEERS))
            })
        };
        list.split(',').map(peer).collect::<Result<_, _>>().map(Some)
    }

    pub fn ssdp(&self) -> Result<Option<SocketAddr>, Error> {
        self.var(hooks::SSDP)
            .map(|address| {
                address.parse().map_err(|_| Error::new(ErrorCode::Usage, format!("{} is not an address", hooks::SSDP)))
            })
            .transpose()
    }

    /// Bytes per second the fake swarm sends.
    pub fn fake_rate(&self) -> u64 {
        self.var(hooks::FAKE_RATE).and_then(parse_size).unwrap_or(10 << 20)
    }

    pub fn dead(&self) -> Vec<TorrentId> {
        self.var(hooks::DEAD).unwrap_or_default().split(',').filter_map(|id| id.parse().ok()).collect()
    }

    pub fn roomy_disk(&self) -> bool {
        self.var(hooks::DISK) == Some("roomy")
    }

    /// A pause `hook` asks for, a minute at most.
    pub fn pause(&self, hook: &str) -> Duration {
        self.duration(hook).map_or(Duration::ZERO, |pause| pause.min(MAX_PAUSE))
    }

    /// The `name=url` pairs in `hook`, `ENDPOINTS` or `SUBTITLE_ENDPOINTS`.
    pub fn endpoints(&self, hook: &str) -> Result<BTreeMap<String, String>, Error> {
        let Some(list) = self.var(hook) else { return Ok(BTreeMap::new()) };
        list.split(',')
            .map(|pair| {
                pair.split_once('=')
                    .map(|(name, url)| (name.trim().to_string(), url.trim().to_string()))
                    .ok_or_else(|| Error::new(ErrorCode::Usage, format!("{hook} has {pair:?}, which is not name=url")))
            })
            .collect()
    }

    pub fn http_port(&self) -> Option<u16> {
        self.var(hooks::HTTP_PORT).and_then(|port| port.parse().ok())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_agents_by_their_variables() {
        let agent = |pairs: &[(&str, &str)]| Env::from_pairs(pairs.iter().copied()).agent();
        assert_eq!(agent(&[("CLAUDECODE", "1")]), Some(Agent::ClaudeCode));
        assert_eq!(agent(&[("AI_AGENT", "claude-code_2-1-301_agent")]), Some(Agent::ClaudeCode));
        assert_eq!(agent(&[("CODEX_CI", "1")]), Some(Agent::Codex));
        assert_eq!(agent(&[("AI_AGENT", "amp")]), Some(Agent::Other));
        assert_eq!(agent(&[("CLAUDECODE", "")]), None);
        assert_eq!(agent(&[]), None);
    }

    #[test]
    fn only_a_process_a_test_started_reads_test_settings() {
        let env = |pairs: &[(&str, &str)]| Env::from_pairs(pairs.iter().copied());
        let pause = |pairs: &[(&str, &str)]| env(pairs).test().map(|test| test.pause(hooks::DELETION_PAUSE));
        let held = (hooks::DELETION_PAUSE, "30s");
        assert_eq!(pause(&[held]), None);
        assert_eq!(
            pause(&[held, (hooks::NETWORK, "public")]),
            None,
            "an engine on the public network is no test engine"
        );
        assert_eq!(pause(&[held, (hooks::NETWORK, "loopback")]), Some(Duration::from_secs(30)));
        let long = (hooks::DELETION_PAUSE, "2h");
        assert_eq!(pause(&[long, (hooks::ENGINE, "fake")]), Some(Duration::from_secs(60)), "and a minute at most");
        let older = (hooks::VERSION, "0.1.0");
        assert_eq!(env(&[older]).version(), env!("CARGO_PKG_VERSION"));
        assert_eq!(env(&[older, (hooks::ENGINE, "fake")]).version(), "0.1.0");
    }

    #[test]
    fn a_codex_sandbox_mark_names_codex_inside_another_harness() {
        let env = Env::from_pairs([("CLAUDECODE", "1"), ("CODEX_SANDBOX", "seatbelt")]);
        assert_eq!(env.agent(), Some(Agent::ClaudeCode));
        assert_eq!(env.sandbox_harness(), Some(Agent::Codex));
    }
}
