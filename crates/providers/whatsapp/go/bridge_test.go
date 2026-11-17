package main

import (
	"context"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"go.mau.fi/whatsmeow/appstate"
	waProto "go.mau.fi/whatsmeow/binary/proto"
	"go.mau.fi/whatsmeow/proto/waHistorySync"
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

func TestConversationUnreadCountHonorsMarkedAsUnread(t *testing.T) {
	// A genuine unread count is reported verbatim.
	withCount := &waHistorySync.Conversation{UnreadCount: proto.Uint32(4)}
	if got := conversationUnreadCount(withCount); got != 4 {
		t.Fatalf("expected reported unread count 4, got %d", got)
	}

	// A manual "mark as unread" with no unread messages still shows as unread.
	markedOnly := &waHistorySync.Conversation{
		UnreadCount:    proto.Uint32(0),
		MarkedAsUnread: proto.Bool(true),
	}
	if got := conversationUnreadCount(markedOnly); got != 1 {
		t.Fatalf("expected marked-as-unread chat to report 1, got %d", got)
	}

	// A genuine count is not overridden by the manual flag.
	both := &waHistorySync.Conversation{
		UnreadCount:    proto.Uint32(3),
		MarkedAsUnread: proto.Bool(true),
	}
	if got := conversationUnreadCount(both); got != 3 {
		t.Fatalf("expected real unread count 3 to win, got %d", got)
	}

	// A fully-read chat reports zero.
	read := &waHistorySync.Conversation{UnreadCount: proto.Uint32(0)}
	if got := conversationUnreadCount(read); got != 0 {
		t.Fatalf("expected read chat to report 0, got %d", got)
	}

	if got := conversationUnreadCount(nil); got != 0 {
		t.Fatalf("expected nil conversation to report 0, got %d", got)
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

func TestParseMentionedJIDsSplitsAndTrims(t *testing.T) {
	// Rust passes a comma-separated list; empty entries are dropped so a
	// trailing separator never produces a bogus JID.
	got := parseMentionedJIDs(" 123@s.whatsapp.net , 456@s.whatsapp.net ,")
	want := []string{"123@s.whatsapp.net", "456@s.whatsapp.net"}
	if len(got) != len(want) {
		t.Fatalf("expected %v, got %v", want, got)
	}
	for i := range want {
		if got[i] != want[i] {
			t.Fatalf("expected %v, got %v", want, got)
		}
	}
}

func TestParseMentionedJIDsEmptyYieldsNil(t *testing.T) {
	// An empty list must preserve the previous no-mention behavior.
	if got := parseMentionedJIDs("   "); got != nil {
		t.Fatalf("expected nil, got %v", got)
	}
}

func TestBuildTextMessageSetsMentionedJID(t *testing.T) {
	// A mention list upgrades a plain conversation to an ExtendedTextMessage
	// so the MentionedJID metadata reaches the recipient.
	mentioned := []string{"123@s.whatsapp.net"}
	message := buildTextMessage(nil, "hi @123", "", "", "", mentioned)
	if message.GetExtendedTextMessage() == nil {
		t.Fatal("expected an extended text message when mentions are present")
	}
	got := message.GetExtendedTextMessage().GetContextInfo().GetMentionedJID()
	if len(got) != 1 || got[0] != "123@s.whatsapp.net" {
		t.Fatalf("expected mentioned JID to be set, got %v", got)
	}
}

func TestBuildTextMessageWithoutMentionsStaysPlain(t *testing.T) {
	// No reply and no mentions must keep the plain Conversation form.
	message := buildTextMessage(nil, "hi", "", "", "", nil)
	if message.GetConversation() != "hi" {
		t.Fatalf("expected plain conversation, got %v", message)
	}
	if message.GetExtendedTextMessage() != nil {
		t.Fatal("expected no extended text message without mentions")
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

// editWireMessage builds the payload whatsmeow's BuildEdit sends for an edit.
func editWireMessage(targetID, text string, editedAtMS int64) *waProto.Message {
	return &waProto.Message{EditedMessage: &waProto.FutureProofMessage{Message: &waProto.Message{
		ProtocolMessage: &waProto.ProtocolMessage{
			Key:           &waProto.MessageKey{FromMe: proto.Bool(true), ID: proto.String(targetID)},
			Type:          waProto.ProtocolMessage_MESSAGE_EDIT.Enum(),
			EditedMessage: &waProto.Message{Conversation: proto.String(text)},
			TimestampMS:   proto.Int64(editedAtMS),
		},
	}}}
}

func TestDetectMessageEditLivePayload(t *testing.T) {
	editedAt := time.Date(2026, 9, 29, 12, 0, 5, 0, time.UTC)
	evt := &events.Message{
		Info:       types.MessageInfo{ID: "EDIT-STANZA", Timestamp: editedAt.Add(time.Second)},
		RawMessage: editWireMessage("ORIG-1", "fixed typo", editedAt.UnixMilli()),
	}
	evt.UnwrapRaw() // what whatsmeow does for live messages

	edit, ok := detectMessageEdit(evt)
	if !ok {
		t.Fatal("live MESSAGE_EDIT must be detected")
	}
	if edit.targetID != "ORIG-1" {
		t.Fatalf("edit must target the original message, got %q", edit.targetID)
	}
	if !edit.editedAt.Equal(editedAt) {
		t.Fatalf("edit time must come from TimestampMS, got %s", edit.editedAt)
	}
	event, ok := buildEditEvent(nil, context.Background(), evt, edit, types.NewJID("40700000000", types.DefaultUserServer), "", "", false)
	if !ok || event.Type != "edit" || event.ID != "ORIG-1" || event.Text != "fixed typo" || event.EditedAt == "" {
		t.Fatalf("unexpected edit event: %+v", event)
	}
}

func TestDetectMessageEditHistoryParsedPayload(t *testing.T) {
	editedAt := time.Date(2026, 9, 29, 12, 0, 5, 0, time.UTC)
	evt := &events.Message{
		Info:       types.MessageInfo{ID: "EDIT-STANZA", Timestamp: editedAt},
		RawMessage: editWireMessage("ORIG-2", "history edit", editedAt.UnixMilli()),
	}
	// Mirror ParseWebMessage: unwrap, then rewrite id and content in place.
	evt.UnwrapRaw()
	evt.Info.ID = evt.Message.GetProtocolMessage().GetKey().GetID()
	evt.Message = evt.Message.GetProtocolMessage().GetEditedMessage()

	edit, ok := detectMessageEdit(evt)
	if !ok {
		t.Fatal("history-parsed edit must be detected, not replayed as a new message")
	}
	if edit.targetID != "ORIG-2" {
		t.Fatalf("history edit must keep the original id, got %q", edit.targetID)
	}
	if text, ok := editedText(edit.content); !ok || text != "history edit" {
		t.Fatalf("unexpected edited text %q ok=%v", text, ok)
	}

	// The edit's timestamp must not count as conversation activity.
	older := editedAt.Add(-time.Hour)
	ts, preview := conversationActivity(nil, context.Background(), []*events.Message{
		historyTestMessage(older, &waProto.Message{Conversation: proto.String("original")}),
		evt,
	}, 0)
	if !ts.Equal(older) || preview != "original" {
		t.Fatalf("history edit leaked into activity: ts=%s preview=%q", ts, preview)
	}
}

func TestDetectMessageEditIgnoresPlainAndRevokeMessages(t *testing.T) {
	plain := &events.Message{RawMessage: &waProto.Message{Conversation: proto.String("hello")}}
	plain.UnwrapRaw()
	if _, ok := detectMessageEdit(plain); ok {
		t.Fatal("a normal text message is not an edit")
	}

	revoke := &events.Message{RawMessage: &waProto.Message{EditedMessage: &waProto.FutureProofMessage{Message: &waProto.Message{
		ProtocolMessage: &waProto.ProtocolMessage{
			Key:  &waProto.MessageKey{ID: proto.String("X")},
			Type: waProto.ProtocolMessage_REVOKE.Enum(),
		},
	}}}}
	revoke.UnwrapRaw()
	if _, ok := detectMessageEdit(revoke); ok {
		t.Fatal("an admin revoke shares the wrapper but is not an edit")
	}
}

func TestEditedTextSupportsCaptionsOnly(t *testing.T) {
	caption := &waProto.Message{ImageMessage: &waProto.ImageMessage{Caption: proto.String("new caption")}}
	if text, ok := editedText(caption); !ok || text != "new caption" {
		t.Fatalf("caption edits must be forwarded, got %q ok=%v", text, ok)
	}
	sticker := &waProto.Message{StickerMessage: &waProto.StickerMessage{}}
	if _, ok := editedText(sticker); ok {
		t.Fatal("non-text edits must be ignored")
	}
}

func TestIsOwnParticipantMatchesPhoneNumberAndLIDIgnoringDevice(t *testing.T) {
	ownPN := types.NewADJID("40711111111", 0, 12)
	ownLID := types.NewJID("123456789", types.HiddenUserServer)

	if !isOwnParticipant(ownPN, ownLID, types.NewJID("40711111111", types.DefaultUserServer)) {
		t.Fatal("phone-number participant should match own JID without the device suffix")
	}
	if !isOwnParticipant(ownPN, ownLID, types.EmptyJID, types.NewJID("123456789", types.HiddenUserServer)) {
		t.Fatal("LID participant should match own LID")
	}
	other := types.NewJID("40722222222", types.DefaultUserServer)
	if isOwnParticipant(ownPN, ownLID, other, types.NewJID("987654321", types.HiddenUserServer)) {
		t.Fatal("another participant must not be marked as self")
	}
	// Same user digits in the other namespace must not match.
	if isOwnParticipant(ownPN, ownLID, types.NewJID("40711111111", types.HiddenUserServer)) {
		t.Fatal("identities are compared within their own namespace")
	}
	if isOwnParticipant(types.EmptyJID, types.EmptyJID, other) {
		t.Fatal("without a signed-in identity nobody is self")
	}
}

func TestMessageQuotedReplyDMTextDefaultsSenderToChat(t *testing.T) {
	chat := types.NewJID("40747086008", types.DefaultUserServer)
	message := &waProto.Message{ExtendedTextMessage: &waProto.ExtendedTextMessage{
		Text: proto.String("Voiam să-ți amintesc de asta"),
		ContextInfo: &waProto.ContextInfo{
			StanzaID:      proto.String("ORIGINAL1"),
			QuotedMessage: &waProto.Message{Conversation: proto.String("  original\n text ")},
		},
	}}
	reply, ok := messageQuotedReply(message, chat)
	if !ok {
		t.Fatal("expected a quoted reply")
	}
	if reply.ID != "ORIGINAL1" || reply.SenderJID != chat || reply.Text != "original text" {
		t.Fatalf("unexpected reply %+v", reply)
	}
}

func TestMessageQuotedReplyGroupUsesParticipant(t *testing.T) {
	chat := types.NewJID("120363157988526102", types.GroupServer)
	message := &waProto.Message{ImageMessage: &waProto.ImageMessage{
		Caption: proto.String("look"),
		ContextInfo: &waProto.ContextInfo{
			StanzaID:      proto.String("Q2"),
			Participant:   proto.String("36786512371803@lid"),
			QuotedMessage: &waProto.Message{AudioMessage: &waProto.AudioMessage{PTT: proto.Bool(true)}},
		},
	}}
	reply, ok := messageQuotedReply(message, chat)
	if !ok {
		t.Fatal("expected a quoted reply in an image caption")
	}
	if reply.SenderJID.String() != "36786512371803@lid" || reply.Text != "Voice message" {
		t.Fatalf("unexpected reply %+v", reply)
	}
}

func TestMessageQuotedReplyUnwrapsEphemeral(t *testing.T) {
	chat := types.NewJID("1", types.DefaultUserServer)
	message := &waProto.Message{EphemeralMessage: &waProto.FutureProofMessage{Message: &waProto.Message{
		ExtendedTextMessage: &waProto.ExtendedTextMessage{
			Text:        proto.String("hi"),
			ContextInfo: &waProto.ContextInfo{StanzaID: proto.String("Q3"), QuotedMessage: &waProto.Message{ImageMessage: &waProto.ImageMessage{}}},
		},
	}}}
	reply, ok := messageQuotedReply(message, chat)
	if !ok || reply.ID != "Q3" || reply.Text != "Photo" {
		t.Fatalf("unexpected reply %+v ok=%v", reply, ok)
	}
}

func TestMessageQuotedReplyIgnoresPlainStatusAndEmpty(t *testing.T) {
	chat := types.NewJID("1", types.DefaultUserServer)
	if _, ok := messageQuotedReply(&waProto.Message{Conversation: proto.String("plain")}, chat); ok {
		t.Fatal("plain message must not be a reply")
	}
	mentionOnly := &waProto.Message{ExtendedTextMessage: &waProto.ExtendedTextMessage{
		Text: proto.String("@1"), ContextInfo: &waProto.ContextInfo{MentionedJID: []string{"1@s.whatsapp.net"}},
	}}
	if _, ok := messageQuotedReply(mentionOnly, chat); ok {
		t.Fatal("mention-only context must not be a reply")
	}
	status := &waProto.Message{ExtendedTextMessage: &waProto.ExtendedTextMessage{
		Text: proto.String("nice"), ContextInfo: &waProto.ContextInfo{StanzaID: proto.String("S"), RemoteJID: proto.String("status@broadcast")},
	}}
	if _, ok := messageQuotedReply(status, chat); ok {
		t.Fatal("status replies must be ignored")
	}
}

func TestApplyQuotedReplyMarksOwnQuoteAndForwarded(t *testing.T) {
	chat := types.NewJID("1", types.DefaultUserServer)
	payload := &waProto.Message{ExtendedTextMessage: &waProto.ExtendedTextMessage{
		Text: proto.String("reply"),
		ContextInfo: &waProto.ContextInfo{
			StanzaID:      proto.String("Q4"),
			QuotedMessage: &waProto.Message{DocumentMessage: &waProto.DocumentMessage{FileName: proto.String("cv.pdf")}},
			IsForwarded:   proto.Bool(true),
		},
	}}
	var event bridgeEvent
	applyQuotedReply(nil, context.Background(), nil, payload, chat, &event)
	if event.ReplyToID != "Q4" || event.ReplyToText != "cv.pdf" || !event.IsForwarded {
		t.Fatalf("unexpected event %+v", event)
	}
	if event.ReplyToSenderJID != chat.String() {
		t.Fatalf("expected DM quote sender to be the chat peer, got %q", event.ReplyToSenderJID)
	}
}

func TestMessageMentionedJIDCoversPollAndLocation(t *testing.T) {
	poll := &waProto.Message{PollCreationMessageV3: &waProto.PollCreationMessage{
		Name: proto.String("q"), ContextInfo: &waProto.ContextInfo{MentionedJID: []string{"2@s.whatsapp.net"}},
	}}
	if got := messageMentionedJID(poll); len(got) != 1 {
		t.Fatalf("expected poll mention, got %v", got)
	}
	loc := &waProto.Message{LocationMessage: &waProto.LocationMessage{ContextInfo: &waProto.ContextInfo{StanzaID: proto.String("L")}}}
	if _, ok := messageQuotedReply(loc, types.NewJID("1", types.DefaultUserServer)); !ok {
		t.Fatal("expected location reply to be detected")
	}
}

func TestChatUnreadEventMarkedUnreadForcesBadgeAndCarriesAsOf(t *testing.T) {
	chat := types.NewJID("40700000000", types.DefaultUserServer)
	at := time.Date(2026, 10, 8, 9, 30, 0, 0, time.UTC)
	event := chatUnreadEvent(chat, 0, at, true)
	if event.Type != "chat_unread" || event.UnreadCount == nil || *event.UnreadCount != 1 || !event.MarkedUnread {
		t.Fatalf("unexpected marked-unread event %+v", event)
	}
	if event.AsOf != "2026-10-08T09:30:00Z" {
		t.Fatalf("unexpected as_of %q", event.AsOf)
	}
	plain := chatUnreadEvent(chat, 3, time.Time{}, false)
	if *plain.UnreadCount != 3 || plain.MarkedUnread || plain.AsOf != "" {
		t.Fatalf("unexpected plain event %+v", plain)
	}
}

func TestChatReadEventCarriesReadUpTo(t *testing.T) {
	chat := types.NewJID("1", types.DefaultUserServer)
	event := chatReadEvent(chat, time.Date(2026, 10, 8, 9, 0, 0, 0, time.UTC))
	if event.Type != "read" || event.ReadUpTo != "2026-10-08T09:00:00Z" {
		t.Fatalf("unexpected read event %+v", event)
	}
	if chatReadEvent(chat, time.Time{}).ReadUpTo != "" {
		t.Fatal("zero time must not be serialized")
	}
}

func TestParseMarkReadRequestAcceptsLegacyArrayAndObject(t *testing.T) {
	legacy, err := parseMarkReadRequest(`[{"id":"A","sender_jid":"1@s.whatsapp.net"}]`)
	if err != nil || len(legacy.Entries) != 1 || legacy.ClearMarkedUnread {
		t.Fatalf("unexpected legacy parse %+v err=%v", legacy, err)
	}
	object, err := parseMarkReadRequest(`{"entries":[{"id":"B","sender_jid":""}],"clear_marked_unread":true,"last_id":"B","last_timestamp_ms":1700000000000}`)
	if err != nil || len(object.Entries) != 1 || !object.ClearMarkedUnread || object.LastID != "B" {
		t.Fatalf("unexpected object parse %+v err=%v", object, err)
	}
}

func TestBuildClearMarkedUnreadPatchTargetsRegularLow(t *testing.T) {
	chat := types.NewJID("120363157988526102", types.GroupServer)
	patch := buildClearMarkedUnreadPatch(chat, markReadRequest{
		ClearMarkedUnread: true,
		LastID:            "LAST",
		LastSenderJID:     "36786512371803@lid",
		LastTimestampMS:   1_700_000_000_000,
	})
	if patch.Type != appstate.WAPatchRegularLow || len(patch.Mutations) != 1 {
		t.Fatalf("unexpected patch %+v", patch)
	}
	action := patch.Mutations[0].Value.GetMarkChatAsReadAction()
	if !action.GetRead() {
		t.Fatal("patch must mark the chat read")
	}
	messages := action.GetMessageRange().GetMessages()
	if len(messages) != 1 || messages[0].GetKey().GetID() != "LAST" || messages[0].GetKey().GetParticipant() != "36786512371803@lid" {
		t.Fatalf("unexpected message range %+v", action.GetMessageRange())
	}
}

func TestProfilePictureCacheKeySeparatesPreviewAndFull(t *testing.T) {
	jid := "447700900123@s.whatsapp.net"
	preview := profilePictureCacheKey("preview", jid, "pic-1", "https://example/p")
	full := profilePictureCacheKey("full", jid, "pic-1", "https://example/f")
	if preview == full {
		t.Fatalf("preview and full avatars share cache key %q", preview)
	}
	if got := profilePictureCacheKey("", jid, "pic-1", ""); got != full {
		t.Fatalf("empty variant should default to full: got %q want %q", got, full)
	}
	if got := profilePictureCacheKey("full", jid, "", "https://example/f"); !strings.Contains(got, "https://example/f") {
		t.Fatalf("missing picture id should key on url: %q", got)
	}
}

func TestPresenceEventRecordsLatestSnapshot(t *testing.T) {
	c := &client{dbPath: "test:presence"}
	jid := types.NewJID("447700900123", types.DefaultUserServer)
	seen := time.Date(2026, 1, 2, 3, 4, 5, 0, time.UTC)
	emitPresenceEvent(c, &events.Presence{From: jid, Unavailable: true, LastSeen: seen})
	got, ok := c.presenceLatest[jid.String()]
	if !ok {
		t.Fatalf("presence snapshot not recorded: %#v", c.presenceLatest)
	}
	if got.PresenceState != "offline" || got.LastSeen != "2026-01-02T03:04:05Z" {
		t.Fatalf("unexpected presence snapshot: %#v", got)
	}
	emitPresenceEvent(c, &events.Presence{From: jid})
	if got := c.presenceLatest[jid.String()]; got.PresenceState != "online" || got.LastSeen != "" {
		t.Fatalf("online update should replace snapshot: %#v", got)
	}
}
