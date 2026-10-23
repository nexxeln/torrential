# The tapes in this folder source this before they record. torrential prints its
# plain agent output when it sees an agent's variables, so clear every one that
# crates/torrential/src/env.rs checks. A scratch home keeps the demo engine and
# its downloads away from yours, and the config turns off the general indexes so
# only lawful sources appear.
unset CLAUDECODE CLAUDE_CODE_CHILD_SESSION CODEX_THREAD_ID CODEX_CI GEMINI_CLI CURSOR_AGENT AI_AGENT \
	CODEX_SANDBOX CODEX_SANDBOX_NETWORK_DISABLED
export PATH="$PWD/target/release:$PATH" TORRENTIAL_HOME=/tmp/torrential-demo/home COLORTERM=truecolor
torrential engine stop >/dev/null 2>&1
rm -rf /tmp/torrential-demo
mkdir -p "$TORRENTIAL_HOME/config"
cat >"$TORRENTIAL_HOME/config/config.yaml" <<'YAML'
downloads:
  dir: /tmp/torrential-demo/downloads
sources:
  disabled: [apibay, knaben, nyaa]
YAML
PS1='\[\e[38;2;166;199;154m\]$\[\e[0m\] '
clear
