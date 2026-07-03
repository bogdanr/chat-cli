package main

import (
	"context"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	waProto "go.mau.fi/whatsmeow/binary/proto"
	"go.mau.fi/whatsmeow/types"
	"go.mau.fi/whatsmeow/types/events"
	"google.golang.org/protobuf/proto"
)

func TestWaLoggerWritesWhatsmeowLogsToFile(t *testing.T) {
	logPath := filepath.Join(t.TempDir(), "debug.log")
	t.Setenv("CHATCLI_WHATSAPP_LOG", "info")
	c := &client{logPath: logPath}

	logger := c.waLogger()
	logger.Sub("Client").Infof("connected to %s", "whatsapp")
	logger.Sub("Client").Debugf("verbose %d", 1)

	data, err := os.ReadFile(logPath)
	if err != nil {
		t.Fatalf("reading log file: %v", err)
	}
	contents := string(data)
	if !strings.Contains(contents, "whatsmeow logging enabled at level=INFO") {
		t.Fatalf("expected logger to announce it is enabled, got:\n%s", contents)
	}
	if !strings.Contains(contents, "whatsmeow [whatsmeow/Client INFO] connected to whatsapp") {
		t.Fatalf("expected info log to be forwarded, got:\n%s", contents)
	}
	if strings.Contains(contents, "verbose 1") {
		t.Fatalf("debug log must be suppressed below the info threshold, got:\n%s", contents)
	}
}

func TestWaLoggerDisabledWithoutEnv(t *testing.T) {
	logPath := filepath.Join(t.TempDir(), "debug.log")
	t.Setenv("CHATCLI_WHATSAPP_LOG", "")
	c := &client{logPath: logPath}

	if logger := c.waLogger(); logger == nil {
		t.Fatal("waLogger must always return a usable logger")
	}

	data, _ := os.ReadFile(logPath)
	if !strings.Contains(string(data), "whatsmeow logging disabled") {
		t.Fatalf("expected disabled diagnostic, got:\n%s", string(data))
	}
}

func TestChooseCanonicalJIDPrefersPhoneNumberJID(t *testing.T) {
	phone := types.NewJID("40712345678", types.DefaultUserServer)
	lid := types.NewJID("123456789012345", types.HiddenUserServer)
	group := types.NewJID("120363012345678901", types.GroupServer)

	if got := chooseCanonicalJID(lid, phone); got != phone {
		t.Fatalf("expected LID with phone alternate to canonicalize to phone JID, got %s", got.String())
	}
	if got := chooseCanonicalJID(phone, lid); got != phone {
		t.Fatalf("expected phone JID to remain canonical, got %s", got.String())
	}
	if got := chooseCanonicalJID(group, phone); got != group {
		t.Fatalf("expected group JID to remain unchanged, got %s", got.String())
	}
	if got := chooseCanonicalJID(lid, types.EmptyJID); got != lid {
		t.Fatalf("expected JID without alternate to remain unchanged, got %s", got.String())
	}
}

func TestContactRealNameIgnoresBareJIDFallback(t *testing.T) {
	// A contact that has never resolved a name (e.g. an unsaved group
	// participant or a @lid sender) must report no real name, so the resolver
	// chain falls through to the alternate JID / push name instead of locking
	// in the bare phone number or LID.
	if name := contactRealName(types.ContactInfo{Found: false}); name != "" {
		t.Fatalf("expected empty real name for unknown contact, got %q", name)
	}

	withName := types.ContactInfo{Found: true, FullName: "George Tacciu"}
	if name := contactRealName(withName); name != "George Tacciu" {
		t.Fatalf("expected resolved full name, got %q", name)
	}

	pushOnly := types.ContactInfo{Found: true, PushName: "Sorin"}
	if name := contactRealName(pushOnly); name != "Sorin" {
		t.Fatalf("expected push name to count as a real name, got %q", name)
	}
}

func TestDisplayNameForContactKeepsBareJIDFallbackForSearch(t *testing.T) {
	// displayNameForContact (used for the contact picker) still labels unknown
	// contacts with their bare JID user so search results are never blank.
	lid := types.NewJID("123884204486851", types.HiddenUserServer)
	if name := displayNameForContact(types.ContactInfo{}, lid); name != "123884204486851" {
		t.Fatalf("expected bare JID user fallback for display, got %q", name)
	}

	phone := types.NewJID("34819417346247", types.DefaultUserServer)
	named := types.ContactInfo{Found: true, FirstName: "Bogdan"}
	if name := displayNameForContact(named, phone); name != "Bogdan" {
		t.Fatalf("expected real name to win over bare JID, got %q", name)
	}
}

func TestAddressBookContactNameOnlyAcceptsSavedNames(t *testing.T) {
	// Push names alone must not surface a contact as a sidebar chat: they
	// belong to people who merely messaged the user, not saved contacts.
	pushOnly := types.ContactInfo{Found: true, PushName: "Sorin"}
	if name := addressBookContactName(pushOnly); name != "" {
		t.Fatalf("expected push-name-only contact to be skipped, got %q", name)
	}

	saved := types.ContactInfo{Found: true, FullName: "Ada Lovelace", PushName: "ada"}
	if name := addressBookContactName(saved); name != "Ada Lovelace" {
		t.Fatalf("expected saved full name, got %q", name)
	}

	firstNameOnly := types.ContactInfo{Found: true, FirstName: " Grace "}
	if name := addressBookContactName(firstNameOnly); name != "Grace" {
		t.Fatalf("expected trimmed first name, got %q", name)
	}

	business := types.ContactInfo{Found: true, BusinessName: "Hopper Computing"}
	if name := addressBookContactName(business); name != "Hopper Computing" {
		t.Fatalf("expected business name, got %q", name)
	}
}

func historyTestMessage(ts time.Time, message *waProto.Message) *events.Message {
	return &events.Message{
		Info:    types.MessageInfo{MessageSource: types.MessageSource{}, Timestamp: ts},
		Message: message,
	}
}

func TestConversationActivityUsesNewestPayloadMessage(t *testing.T) {
	older := time.Date(2026, 6, 1, 10, 0, 0, 0, time.UTC)
	newer := time.Date(2026, 6, 9, 18, 30, 0, 0, time.UTC)
	messages := []*events.Message{
		historyTestMessage(older, &waProto.Message{Conversation: proto.String("first")}),
		historyTestMessage(newer, &waProto.Message{Conversation: proto.String("latest")}),
	}

	gotTs, gotPreview := conversationActivity(nil, context.Background(), messages, 0)
	if !gotTs.Equal(newer) {
		t.Fatalf("expected newest message timestamp %s, got %s", newer, gotTs)
	}
	if gotPreview != "latest" {
		t.Fatalf("expected preview of newest message, got %q", gotPreview)
	}
}

func TestConversationActivityFallsBackToConversationTimestamp(t *testing.T) {
	// INITIAL_BOOTSTRAP conversations often arrive with metadata only; the
	// conversation's last-message timestamp must still mark the chat as
	// recently active so it is not shown as never contacted.
	last := time.Date(2026, 6, 8, 9, 0, 0, 0, time.UTC)
	gotTs, gotPreview := conversationActivity(nil, context.Background(), nil, uint64(last.Unix()))
	if !gotTs.Equal(last) {
		t.Fatalf("expected fallback timestamp %s, got %s", last, gotTs)
	}
	if gotPreview != "" {
		t.Fatalf("expected no preview for metadata-only fallback, got %q", gotPreview)
	}

	gotTs, _ = conversationActivity(nil, context.Background(), nil, 0)
	if !gotTs.IsZero() {
		t.Fatalf("expected zero time when no activity is known, got %s", gotTs)
	}
}

func TestConversationActivitySkipsNonDisplayableMessages(t *testing.T) {
	older := time.Date(2026, 6, 1, 10, 0, 0, 0, time.UTC)
	newer := time.Date(2026, 6, 9, 18, 30, 0, 0, time.UTC)
	reaction := &waProto.Message{ReactionMessage: &waProto.ReactionMessage{Text: proto.String("👍")}}
	messages := []*events.Message{
		historyTestMessage(older, &waProto.Message{Conversation: proto.String("real text")}),
		historyTestMessage(newer, reaction),
	}

	gotTs, gotPreview := conversationActivity(nil, context.Background(), messages, 0)
	if !gotTs.Equal(older) {
		t.Fatalf("expected reaction to be skipped in favour of %s, got %s", older, gotTs)
	}
	if gotPreview != "real text" {
		t.Fatalf("expected preview from displayable message, got %q", gotPreview)
	}
}

func TestDisplayableMessageUnwrapsEphemeralText(t *testing.T) {
	message := &waProto.Message{
		EphemeralMessage: &waProto.FutureProofMessage{
			Message: &waProto.Message{
				ExtendedTextMessage: &waProto.ExtendedTextMessage{Text: proto.String("visible text")},
			},
		},
	}

	if text := messageText(message); text != "visible text" {
		t.Fatalf("expected wrapped text to be visible, got %q", text)
	}
}

func TestDisplayableMessageUnwrapsViewOncePhoto(t *testing.T) {
	message := &waProto.Message{
		ViewOnceMessageV2: &waProto.FutureProofMessage{
			Message: &waProto.Message{
				ImageMessage: &waProto.ImageMessage{
					Mimetype:   proto.String("image/jpeg"),
					Caption:    proto.String("photo caption"),
					DirectPath: proto.String("/v/t62/photo.enc"),
				},
			},
		},
	}
	event := bridgeEvent{Text: messageText(message)}

	applyMedia(nil, "photo-1", message, &event)

	if event.ContentType != "image" {
		t.Fatalf("expected wrapped photo content type, got %q", event.ContentType)
	}
	if event.Text != "photo caption" {
		t.Fatalf("expected wrapped photo caption, got %q", event.Text)
	}
	if event.MediaID == "" {
		t.Fatal("expected wrapped photo to receive a media id")
	}
}

func TestRewriteMentionTokensReplacesUserPartsWithNames(t *testing.T) {
	// WhatsApp carries the mention as the bare JID user-part in the body; the
	// UI must show the contact name instead of the raw number.
	text := "chiar @34819417346247 , care a fost root cause-ul?"
	names := map[string]string{"34819417346247": "Razvan"}
	got := rewriteMentionTokens(text, names)
	want := "chiar @Razvan , care a fost root cause-ul?"
	if got != want {
		t.Fatalf("expected %q, got %q", want, got)
	}
}

func TestRewriteMentionTokensAppliesLongestUserPartFirst(t *testing.T) {
	// A shorter number that is a prefix of a longer one must not corrupt the
	// longer mention, so replacement happens longest user-part first.
	text := "@123 and @12345"
	names := map[string]string{"123": "Ana", "12345": "Bob"}
	got := rewriteMentionTokens(text, names)
	want := "@Ana and @Bob"
	if got != want {
		t.Fatalf("expected %q, got %q", want, got)
	}
}

func TestRewriteMentionTokensLeavesUnknownMentionsUnchanged(t *testing.T) {
	// An unresolved JID is left as-is so the mention is never dropped.
	text := "hi @999"
	if got := rewriteMentionTokens(text, map[string]string{}); got != text {
		t.Fatalf("expected text unchanged, got %q", got)
	}
}

func TestOfflineSyncWindowTogglesBacklogFlag(t *testing.T) {
	// The offline-sync window brackets the server's replay of events missed
	// during downtime. While it is open, messages must be classified as
	// backlog so consumers stay silent; once it closes, messages are live
	// again. These events never touch the whatsmeow client, so a bare client
	// value is enough to exercise the state machine.
	c := &client{}
	if c.offlineSync.Load() {
		t.Fatal("offline sync flag must start cleared")
	}

	handleWhatsAppEvent(c, &events.OfflineSyncPreview{Messages: 3})
	if !c.offlineSync.Load() {
		t.Fatal("expected offline sync flag to be set after OfflineSyncPreview")
	}

	handleWhatsAppEvent(c, &events.OfflineSyncCompleted{Count: 3})
	if c.offlineSync.Load() {
		t.Fatal("expected offline sync flag to be cleared after OfflineSyncCompleted")
	}
}
