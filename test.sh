cargo build --workspace
./target/debug/chat-cli \
  --log-file tmp/chat-cli-debug.log \
  --db tmp/chat-cli.sqlite \
  --whatsapp-db tmp/chat-cli-whatsapp-session.db
