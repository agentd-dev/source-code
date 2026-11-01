# shellcheck shell=sh
# Source this before starting the demo agents:  . examples/demos/env.sh
#
# Makes the demo's local credentials, once, under $AGENTD_DEMO_HOME/tokens
# (mode 600), and exports them for agentd's {{secret:…}} references:
#   CONTROLLER_TOKEN   Morgan Ellis, financial controller (answers gates)
#   CFO_TOKEN          Sam Okafor, CFO (answers gates above GBP 50,000)
#   RECON_PEER_TOKEN   the close agent's credential at the recon agent
#   SUPPORT_TOKEN      Harbor Bank's support desk, at the three channels (demo c)
#   ENGINEER_TOKEN     the engineer at the coding agent (demo d)
#   NATALIIA_TOKEN     Nataliia, at the climate assistant (demo e)
#   ANDRII_TOKEN       Andrii, at the climate assistant (demo e)
#   HOME_PEER_TOKEN    the climate assistant's credential at the home agent (demo e)
# The TUI reads the same files with --bearer-file. These are loopback demo
# tokens with no value anywhere else; delete the folder to rotate them.
#
# It does not touch OPENAI_API_KEY or the registry secrets
# (INSTRUCTION_MD_CLIENT_SECRET, CLOSE_CLIENT_SECRET, CHAT_/VOICE_/EMAIL_AGENT_CLIENT_SECRET,
# CODER_CLIENT_SECRET, HOME_CLIENT_SECRET): export those yourself, from
# wherever you keep them.

export AGENTD_DEMO_HOME="${AGENTD_DEMO_HOME:-/tmp/agentd-demos}"
_demo_tokens="$AGENTD_DEMO_HOME/tokens"
mkdir -p "$_demo_tokens"
for _demo_name in controller cfo recon-peer support engineer nataliia andrii home-peer; do
  if [ ! -s "$_demo_tokens/$_demo_name.token" ]; then
    (umask 077 && od -An -N24 -tx1 /dev/urandom | tr -d ' \n' > "$_demo_tokens/$_demo_name.token")
  fi
done
CONTROLLER_TOKEN="$(cat "$_demo_tokens/controller.token")"
CFO_TOKEN="$(cat "$_demo_tokens/cfo.token")"
RECON_PEER_TOKEN="$(cat "$_demo_tokens/recon-peer.token")"
SUPPORT_TOKEN="$(cat "$_demo_tokens/support.token")"
ENGINEER_TOKEN="$(cat "$_demo_tokens/engineer.token")"
NATALIIA_TOKEN="$(cat "$_demo_tokens/nataliia.token")"
ANDRII_TOKEN="$(cat "$_demo_tokens/andrii.token")"
HOME_PEER_TOKEN="$(cat "$_demo_tokens/home-peer.token")"
export CONTROLLER_TOKEN CFO_TOKEN RECON_PEER_TOKEN SUPPORT_TOKEN ENGINEER_TOKEN \
  NATALIIA_TOKEN ANDRII_TOKEN HOME_PEER_TOKEN
unset _demo_tokens _demo_name
