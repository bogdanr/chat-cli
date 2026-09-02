#!/bin/bash
cargo build --workspace

source .env
# Official chat-cli Slack app (enables the credential-free "Add to Slack" flow).
# Client ID is public; secrets must NOT be committed. Export them before running:
export CHAT_CLI_SLACK_CLIENT_SECRET=${CHAT_CLI_SLACK_CLIENT_SECRET:-}
export CHAT_CLI_SLACK_CLIENT_ID="4614087544.11325148183808"

# Optional but recommended for realtime Slack delivery. Keep this xapp- token in
# your shell/local env, not in git:
#   export CHAT_CLI_SLACK_APP_TOKEN='xapp-...'

if [ -z "${CHAT_CLI_SLACK_CLIENT_SECRET:-}" ]; then
  echo "warning: CHAT_CLI_SLACK_CLIENT_SECRET is not set — Slack will fall back to manual app setup." >&2
fi
if [ -z "${CHAT_CLI_SLACK_APP_TOKEN:-}" ]; then
  echo "warning: CHAT_CLI_SLACK_APP_TOKEN is not set — Slack realtime will fall back to periodic history checks." >&2
fi

# Surface whatsmeow's own connection/pairing logs in tmp/debug.log so WhatsApp
# link failures can be diagnosed. Unset this (or set to "") to silence it again.
export CHATCLI_WHATSAPP_LOG="${CHATCLI_WHATSAPP_LOG:-debug}"
export CHATCLI_WHATSAPP_CABLE_DUMP=1
export CHAT_CLI_PERF_LOG_FILE=/tmp/slack-diag.log

# ClickUp is opt-in: it only starts when a token is configured here, in .env, or
# previously added from the account screen. Personal tokens never expire, so keep
# CHAT_CLI_CLICKUP_TOKEN in your local env or .env — never in git.
#   export CHAT_CLI_CLICKUP_TOKEN='pk_...'
# Set CHAT_CLI_CLICKUP_WORKSPACE_ID only if the token reaches several workspaces.
clickup_args=()
if [ -n "${CHAT_CLI_CLICKUP_TOKEN:-}" ]; then
  # Naming any provider flag turns off the default-on providers, so re-enable
  # Slack and WhatsApp explicitly to keep this script's behaviour unchanged.
  clickup_args+=(--slack --whatsapp)
  clickup_args+=(--clickup-token "$CHAT_CLI_CLICKUP_TOKEN")
  if [ -n "${CHAT_CLI_CLICKUP_WORKSPACE_ID:-}" ]; then
    clickup_args+=(--clickup-workspace-id "$CHAT_CLI_CLICKUP_WORKSPACE_ID")
  fi
fi


./target/debug/chat-cli \
  --log-file tmp/debug.log \
  --db tmp/chat-cli.sqlite \
  --whatsapp-db tmp/whatsapp.db \
  "${clickup_args[@]}"
