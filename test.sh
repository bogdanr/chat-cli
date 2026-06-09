cargo build --workspace
./target/debug/chat-cli \
  --log-file tmp/debug.log \
  --db tmp/chat-cli.sqlite \
  --whatsapp-db tmp/whatsapp.db
