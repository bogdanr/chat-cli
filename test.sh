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

./target/debug/chat-cli \
  --log-file tmp/debug.log \
  --db tmp/chat-cli.sqlite \
  --whatsapp-db tmp/whatsapp.db
