use std::collections::HashMap;

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
        self.var("TORRENTIAL_TEST_VERSION").unwrap_or(env!("CARGO_PKG_VERSION")).to_string()
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
    fn a_codex_sandbox_mark_names_codex_inside_another_harness() {
        let env = Env::from_pairs([("CLAUDECODE", "1"), ("CODEX_SANDBOX", "seatbelt")]);
        assert_eq!(env.agent(), Some(Agent::ClaudeCode));
        assert_eq!(env.sandbox_harness(), Some(Agent::Codex));
    }
}
