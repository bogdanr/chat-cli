cargo build --workspace
./target/debug/chat-cli --whatsapp \
  --whatsapp-sync today \
  --log-file tmp/chat-cli-whatsapp-debug.log \
  --db tmp/chat-cli-whatsapp-test.sqlite \
  --whatsapp-db tmp/chat-cli-whatsapp-session.db
