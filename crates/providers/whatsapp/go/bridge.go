package main

/*
#include <stdint.h>
#include <stdlib.h>

typedef void (*MessageCallback)(char* message, void* user_data);

static inline void call_message_callback(MessageCallback cb, char* message, void* user_data) {
    cb(message, user_data);
}
*/
import "C"
import (
	"context"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"mime"
	"net/http"
	"os"
	"os/exec"
	"path/filepath"
	"sort"
	"strings"
	"sync"
	"sync/atomic"
	"time"
	"unsafe"

	_ "github.com/mattn/go-sqlite3"
	"go.mau.fi/whatsmeow"
	"go.mau.fi/whatsmeow/appstate"
	waProto "go.mau.fi/whatsmeow/binary/proto"
	"go.mau.fi/whatsmeow/proto/waCompanionReg"
	"go.mau.fi/whatsmeow/proto/waHistorySync"
	"go.mau.fi/whatsmeow/store"
	"go.mau.fi/whatsmeow/store/sqlstore"
	"go.mau.fi/whatsmeow/types"
	"go.mau.fi/whatsmeow/types/events"
	waLog "go.mau.fi/whatsmeow/util/log"
	"google.golang.org/protobuf/proto"
)

type client struct {
	dbPath    string
	syncScope string
	logPath   string
	wa        *whatsmeow.Client
	cancel    context.CancelFunc
	// offlineSync is true while the server is replaying events the client
	// missed during downtime (between OfflineSyncPreview and
	// OfflineSyncCompleted). Messages delivered in this window are catch-up
	// backlog, not live arrivals: they must update history and unread state
	// but must never raise an audio/desktop notification, because the user
	// was already alerted on their phone (and may have already read them).
	offlineSync atomic.Bool
	// cableMu guards cableCancel: the cancel/close hook for a live caBLE
	// post-assertion linger session (tunnel WebSocket kept open so the phone
	// can finish its side of the hybrid ceremony; see startCableLinger).
	// Stored on the client so disconnect/logout tears the tunnel down
	// deterministically instead of relying on ctx propagation alone.
	cableMu     sync.Mutex
	cableCancel func()
}

// setCableCancel stores (or clears, with nil) the teardown hook for the active
// caBLE linger session, cancelling any previous one first.
func (c *client) setCableCancel(cancel func()) {
	c.cableMu.Lock()
	prev := c.cableCancel
	c.cableCancel = cancel
	c.cableMu.Unlock()
	if prev != nil {
		prev()
	}
}

// cancelCableSession tears down any live caBLE linger session.
func (c *client) cancelCableSession() {
	c.setCableCancel(nil)
}

// whatsmeowLogLevels mirrors waLog's internal level ranking so the bridge can
// honor a configured minimum level.
var whatsmeowLogLevels = map[string]int{"": -1, "DEBUG": 0, "INFO": 1, "WARN": 2, "ERROR": 3}

// bridgeLogger adapts whatsmeow's waLog.Logger onto the bridge's file logger so
// the actual connection/pairing handshake (otherwise silent) lands in the same
// debug log as the bridge's own events. This is the only way to diagnose why a
// link attempt rotates QR codes and then times out, since whatsmeow swallows
// all detail when given waLog.Noop.
type bridgeLogger struct {
	c      *client
	module string
	min    int
}

func (l *bridgeLogger) outputf(level, msg string, args ...interface{}) {
	if whatsmeowLogLevels[level] < l.min {
		return
	}
	l.c.log("whatsmeow [%s %s] %s", l.module, level, fmt.Sprintf(msg, args...))
}

func (l *bridgeLogger) Errorf(msg string, args ...interface{}) { l.outputf("ERROR", msg, args...) }
func (l *bridgeLogger) Warnf(msg string, args ...interface{})  { l.outputf("WARN", msg, args...) }
func (l *bridgeLogger) Infof(msg string, args ...interface{})  { l.outputf("INFO", msg, args...) }
func (l *bridgeLogger) Debugf(msg string, args ...interface{}) { l.outputf("DEBUG", msg, args...) }
func (l *bridgeLogger) Sub(module string) waLog.Logger {
	return &bridgeLogger{c: l.c, module: l.module + "/" + module, min: l.min}
}

// waLogger returns a whatsmeow logger that writes into the bridge debug log.
//
// It is opt-in via CHATCLI_WHATSAPP_LOG (e.g. "info", "debug") so normal runs
// keep the log small; when unset, or when no log path is configured, whatsmeow
// stays silent (Noop) exactly as before. An unrecognized level defaults to INFO,
// which surfaces connect failures, stream errors, and pairing outcomes without
// the full DEBUG node dump.
func (c *client) waLogger() waLog.Logger {
	level := strings.ToUpper(strings.TrimSpace(os.Getenv("CHATCLI_WHATSAPP_LOG")))
	if level == "" || strings.TrimSpace(c.logPath) == "" {
		c.log("whatsmeow logging disabled (CHATCLI_WHATSAPP_LOG=%q has_log_path=%t)", level, strings.TrimSpace(c.logPath) != "")
		return waLog.Noop
	}
	min, ok := whatsmeowLogLevels[level]
	if !ok {
		min = whatsmeowLogLevels["INFO"]
	}
	c.log("whatsmeow logging enabled at level=%s", level)
	return &bridgeLogger{c: c, module: "whatsmeow", min: min}
}

type bridgeReaction struct {
	Emoji  string   `json:"emoji"`
	Sender []string `json:"senders"`
}

type bridgeContact struct {
	JID        string `json:"jid"`
	Name       string `json:"name"`
	AvatarPath string `json:"avatar_path,omitempty"`
}

// bridgeMember describes one participant of a WhatsApp group, including the
// participant's admin authority so the UI can badge owners/admins.
type bridgeMember struct {
	JID          string `json:"jid"`
	Name         string `json:"name"`
	AvatarPath   string `json:"avatar_path,omitempty"`
	IsAdmin      bool   `json:"is_admin,omitempty"`
	IsSuperAdmin bool   `json:"is_super_admin,omitempty"`
}

type bridgeEvent struct {
	Type         string `json:"type"`
	Event        string `json:"event,omitempty"`
	Reason       string `json:"reason,omitempty"`
	Message      string `json:"message,omitempty"`
	Code         string `json:"code,omitempty"`
	JID          string `json:"jid,omitempty"`
	ID           string `json:"id,omitempty"`
	CanonicalJID string `json:"canonical_jid,omitempty"`
	AltJID       string `json:"alt_jid,omitempty"`
	ChatJID      string `json:"chat_jid,omitempty"`
	ChatName     string `json:"chat_name,omitempty"`
	SenderJID    string `json:"sender_jid,omitempty"`
	SenderName   string `json:"sender_name,omitempty"`
	AvatarPath   string `json:"avatar_path,omitempty"`
	Text         string `json:"text,omitempty"`
	Timestamp    string `json:"timestamp,omitempty"`
	FromMe       bool   `json:"from_me,omitempty"`
	MentionsMe   bool   `json:"mentions_me,omitempty"`
	IsGroup      bool   `json:"is_group,omitempty"`
	Muted        *bool  `json:"muted,omitempty"`
	Progress     uint8  `json:"progress,omitempty"`

	// UnreadCount is the authoritative unread count for a conversation as
	// tracked by WhatsApp itself (history-sync Conversation.UnreadCount). It is
	// a pointer so "0 unread" (an explicit read state) is distinguishable from
	// "not reported" on events that never carry it.
	UnreadCount *uint32 `json:"unread_count,omitempty"`

	LastMessageAt      string `json:"last_message_at,omitempty"`
	LastMessagePreview string `json:"last_message_preview,omitempty"`

	ContentType           string           `json:"content_type,omitempty"`
	MediaID               string           `json:"media_id,omitempty"`
	MediaFileName         string           `json:"media_file_name,omitempty"`
	MediaMime             string           `json:"media_mime,omitempty"`
	MediaSize             uint64           `json:"media_size,omitempty"`
	MediaLocalPath        string           `json:"media_local_path,omitempty"`
	MediaThumbnail        string           `json:"media_thumbnail_path,omitempty"`
	Caption               string           `json:"caption,omitempty"`
	ReactionMessageID     string           `json:"reaction_message_id,omitempty"`
	ReactionMessageFromMe bool             `json:"reaction_message_from_me,omitempty"`
	ReactionEmoji         string           `json:"reaction_emoji,omitempty"`
	Reactions             []bridgeReaction `json:"reactions,omitempty"`
	PollQuestion          string           `json:"poll_question,omitempty"`
	PollOptions           []string         `json:"poll_options,omitempty"`
	PollOptionIDs         []string         `json:"poll_option_ids,omitempty"`
	PollSelectable        uint32           `json:"poll_selectable_options_count,omitempty"`
	PollVoteMessageID     string           `json:"poll_vote_message_id,omitempty"`
	PollVoteOptions       []string         `json:"poll_vote_options,omitempty"`
	Contacts              []bridgeContact  `json:"contacts,omitempty"`
	Members               []bridgeMember   `json:"members,omitempty"`

	GroupTopic               string `json:"group_topic,omitempty"`
	GroupOwner               string `json:"group_owner,omitempty"`
	GroupCreated             string `json:"group_created,omitempty"`
	GroupOnlyAdminsSend      bool   `json:"group_only_admins_send,omitempty"`
	GroupOnlyAdminsEdit      bool   `json:"group_only_admins_edit,omitempty"`
	GroupDisappearingSeconds uint32 `json:"group_disappearing_seconds,omitempty"`

	// EditedAt is set on `edit` events: the time the edit was made, while ID
	// names the original (edited) message and Text carries the new content.
	EditedAt string `json:"edited_at,omitempty"`
}

var (
	mu       sync.Mutex
	clients         = map[uint64]*client{}
	nextID   uint64 = 1
	msgCb    C.MessageCallback
	msgCbCtx unsafe.Pointer
)

//export C_NewClient
func C_NewClient(dbPath *C.char, syncScope *C.char, logPath *C.char) C.uint64_t {
	mu.Lock()
	defer mu.Unlock()

	scope := strings.ToLower(strings.TrimSpace(C.GoString(syncScope)))
	if scope == "" {
		scope = "all"
	}

	id := nextID
	nextID++
	clients[id] = &client{
		dbPath:    C.GoString(dbPath),
		syncScope: scope,
		logPath:   C.GoString(logPath),
	}
	clients[id].log("created WhatsApp bridge client with sync_scope=%s", scope)
	return C.uint64_t(id)
}

//export C_Connect
func C_Connect(clientID C.uint64_t) C.uint8_t {
	mu.Lock()
	c, ok := clients[uint64(clientID)]
	mu.Unlock()
	if !ok {
		emit(bridgeEvent{Type: "error", Message: "unknown WhatsApp bridge client"})
		return 0
	}

	if strings.HasPrefix(c.dbPath, "test:") {
		c.wa = nil
		c.cancel = func() {}
		emit(bridgeEvent{Type: "login", Event: "test-mode"})
		emit(bridgeEvent{Type: "connected", JID: "test-device@s.whatsapp.net"})
		emit(bridgeEvent{Type: "sync", Progress: 100})
		return 1
	}

	if c.wa != nil && c.wa.IsConnected() {
		return 1
	}

	ctx, cancel := context.WithCancel(context.Background())
	logger := c.waLogger()
	// Present as a browser companion. WhatsApp's passkey-protected linking is a
	// web.whatsapp.com feature; the platform type feeds the final
	// encrypted_pairing_request key-derivation salt ("Companion Pairing <type>
	// with ref …", whatsmeow pair-passkey.go) and rides inside the committed
	// CompanionEphemeralIdentity. The stock UNKNOWN/"whatsmeow" identity is an
	// implausible companion for that flow, so advertise Chrome/Linux instead.
	store.DeviceProps.PlatformType = waCompanionReg.DeviceProps_CHROME.Enum()
	store.DeviceProps.Os = proto.String("Chrome")
	container, err := sqlstore.New(ctx, "sqlite3", sqliteDSN(c.dbPath), logger.Sub("Database"))
	if err != nil {
		cancel()
		emit(bridgeEvent{Type: "error", Message: fmt.Sprintf("open WhatsApp store: %v", err)})
		return 0
	}

	device, err := container.GetFirstDevice(ctx)
	if err != nil {
		cancel()
		emit(bridgeEvent{Type: "error", Message: fmt.Sprintf("load WhatsApp device: %v", err)})
		return 0
	}

	wa := whatsmeow.NewClient(device, logger.Sub("Client"))
	wa.AddEventHandler(func(evt interface{}) {
		handleWhatsAppEvent(c, evt)
	})
	c.wa = wa
	c.cancel = cancel

	if wa.Store.ID == nil {
		qrChan, err := wa.GetQRChannel(ctx)
		if err != nil {
			cancel()
			emit(bridgeEvent{Type: "error", Message: fmt.Sprintf("create WhatsApp QR channel: %v", err)})
			return 0
		}

		go func() {
			for evt := range qrChan {
				switch evt.Event {
				case whatsmeow.QRChannelEventCode:
					emit(bridgeEvent{Type: "qr", Code: evt.Code})
				case whatsmeow.QRChannelEventPasskeyRequest:
					c.handlePasskeyRequest(ctx, evt.PasskeyRequest)
				case whatsmeow.QRChannelEventPasskeyResponse:
					c.handlePasskeyConfirmation(ctx, evt.PasskeyConfirmation)
				case whatsmeow.QRChannelEventError:
					msg := "WhatsApp pairing error"
					if evt.Error != nil {
						msg = evt.Error.Error()
					}
					emit(bridgeEvent{Type: "error", Message: msg})
				default:
					emit(bridgeEvent{Type: "login", Event: evt.Event})
				}
			}
		}()
	}

	if err := wa.Connect(); err != nil {
		cancel()
		emit(bridgeEvent{Type: "error", Message: fmt.Sprintf("connect WhatsApp: %v", err)})
		return 0
	}

	if wa.Store.ID != nil {
		emit(bridgeEvent{Type: "connected", JID: wa.Store.ID.String()})
		// Fetch the local user's own profile picture using the device-less JID.
		// The raw store ID carries a device suffix (e.g. "...:74@s.whatsapp.net")
		// which the profile-picture lookup does not resolve, leaving own messages
		// without an avatar. Contact/member avatars already use device-less JIDs,
		// so normalizing here makes the self avatar resolve like everyone else.
		go c.fetchAndEmitProfile(context.Background(), wa.Store.ID.ToNonAD(), "WhatsApp", false)
		go c.syncChatMuteSettings(context.Background())
		go c.emitJoinedGroups(context.Background())
		go c.emitContacts(context.Background())
	}
	emit(bridgeEvent{Type: "sync", Progress: 100})
	return 1
}

//export C_SetMessageCallback
func C_SetMessageCallback(cb C.MessageCallback, userData unsafe.Pointer) {
	mu.Lock()
	defer mu.Unlock()

	msgCb = cb
	msgCbCtx = userData
}

//export C_RequestHistory
func C_RequestHistory(clientID C.uint64_t, chatJID *C.char, oldestMsgID *C.char, oldestFromMe C.uint8_t, oldestTimestampUnix C.int64_t, count C.int) *C.char {
	mu.Lock()
	c, ok := clients[uint64(clientID)]
	mu.Unlock()
	if !ok {
		return cJSON(bridgeEvent{Type: "error", Message: "unknown WhatsApp bridge client"})
	}
	if strings.HasPrefix(c.dbPath, "test:") {
		chat := C.GoString(chatJID)
		messageID := C.GoString(oldestMsgID)
		requested := int(count)
		if requested <= 0 {
			requested = 50
		}
		if strings.Contains(c.dbPath, "backfill") {
			go func() {
				for i := 0; i < requested && i < 3; i++ {
					emit(bridgeEvent{
						Type:       "history",
						ID:         fmt.Sprintf("test-history-%s-%d", messageID, i),
						ChatJID:    chat,
						SenderJID:  chat,
						SenderName: "Ada",
						Text:       fmt.Sprintf("test older history %d", i+1),
						Timestamp:  time.Unix(int64(oldestTimestampUnix)-int64(i+1), 0).UTC().Format(time.RFC3339Nano),
					})
				}
			}()
		}
		return cJSON(bridgeEvent{Type: "history_request", ChatJID: chat, ID: messageID})
	}
	if c.wa == nil || !c.wa.IsConnected() {
		return cJSON(bridgeEvent{Type: "error", Message: "WhatsApp bridge client is not connected"})
	}

	chat, err := types.ParseJID(C.GoString(chatJID))
	if err != nil || chat.IsEmpty() {
		return cJSON(bridgeEvent{Type: "error", Message: fmt.Sprintf("invalid WhatsApp chat JID: %v", err)})
	}
	messageID := C.GoString(oldestMsgID)
	if messageID == "" {
		return cJSON(bridgeEvent{Type: "error", Message: "cannot request WhatsApp history without an anchor message ID"})
	}
	requested := int(count)
	if requested <= 0 {
		requested = 50
	}
	if requested > 100 {
		requested = 100
	}

	anchor := &types.MessageInfo{
		MessageSource: types.MessageSource{
			Chat:     chat,
			Sender:   chat,
			IsFromMe: oldestFromMe != 0,
			IsGroup:  chat.Server == types.GroupServer,
		},
		ID:        messageID,
		Timestamp: time.Unix(int64(oldestTimestampUnix), 0),
	}
	request := c.wa.BuildHistorySyncRequest(anchor, requested)
	if _, err := c.wa.SendPeerMessage(context.Background(), request); err != nil {
		c.log("request WhatsApp on-demand history failed chat=%s anchor=%s count=%d: %v", chat.String(), messageID, requested, err)
		return cJSON(bridgeEvent{Type: "error", Message: fmt.Sprintf("request WhatsApp history: %v", err)})
	}
	canonicalChat := canonicalJID(c, context.Background(), chat)
	c.log("requested WhatsApp on-demand history chat=%s anchor=%s count=%d", chat.String(), messageID, requested)
	return cJSON(bridgeEvent{Type: "history_request", ChatJID: canonicalChat.String(), ID: messageID})
}

// describeSendError rewrites whatsmeow's cryptic internal-state errors into
// actionable text for the status bar. The most common case is a session that
// the phone/server revoked (stream error <conflict type="device_removed"/>):
// whatsmeow deletes the local device, so every later send fails with
// ErrNotLoggedIn ("the store doesn't contain a device JID"), which tells the
// user nothing about how to recover.
func describeSendError(err error) string {
	switch {
	case errors.Is(err, whatsmeow.ErrNotLoggedIn):
		return "WhatsApp session was logged out (device removed); restart chat-cli and scan the QR code to re-link"
	case errors.Is(err, whatsmeow.ErrNotConnected):
		return "WhatsApp is not connected; waiting for the connection to come back"
	default:
		return err.Error()
	}
}

//export C_SendText
func C_SendText(clientID C.uint64_t, chatJID *C.char, text *C.char, replyID *C.char, replyParticipant *C.char, replyText *C.char, mentionedJIDs *C.char) *C.char {
	mu.Lock()
	c, ok := clients[uint64(clientID)]
	mu.Unlock()
	if !ok {
		return cJSON(bridgeEvent{Type: "error", Message: "WhatsApp bridge client is not connected"})
	}

	body := C.GoString(text)
	replyIDRaw := C.GoString(replyID)
	replyParticipantRaw := C.GoString(replyParticipant)
	replyTextRaw := C.GoString(replyText)
	mentioned := parseMentionedJIDs(C.GoString(mentionedJIDs))
	if strings.HasPrefix(c.dbPath, "test:") {
		chat := C.GoString(chatJID)
		return cJSON(bridgeEvent{
			Type:      "sent",
			ID:        fmt.Sprintf("test-sent-%d", time.Now().UnixNano()),
			ChatJID:   chat,
			SenderJID: "test-device@s.whatsapp.net",
			Text:      body,
			Timestamp: time.Now().UTC().Format(time.RFC3339Nano),
			FromMe:    true,
			IsGroup:   strings.Contains(chat, "@g.us"),
		})
	}
	if c.wa == nil {
		return cJSON(bridgeEvent{Type: "error", Message: "WhatsApp bridge client is not connected"})
	}

	jid, err := types.ParseJID(C.GoString(chatJID))
	if err != nil {
		return cJSON(bridgeEvent{Type: "error", Message: fmt.Sprintf("invalid WhatsApp chat JID: %v", err)})
	}

	resp, err := c.wa.SendMessage(context.Background(), jid, buildTextMessage(c, body, replyIDRaw, replyParticipantRaw, replyTextRaw, mentioned))
	if err != nil {
		return cJSON(bridgeEvent{Type: "error", Message: fmt.Sprintf("send WhatsApp message: %s", describeSendError(err))})
	}

	id := resp.ID
	if id == "" {
		id = string(whatsmeow.GenerateMessageID())
	}
	canonicalChat := canonicalJID(c, context.Background(), jid)
	return cJSON(bridgeEvent{
		Type:      "sent",
		ID:        id,
		ChatJID:   canonicalChat.String(),
		SenderJID: c.ownJID(),
		Text:      body,
		Timestamp: time.Now().UTC().Format(time.RFC3339Nano),
		FromMe:    true,
		IsGroup:   strings.HasSuffix(jid.Server, "g.us"),
	})
}

//export C_SendMedia
func C_SendMedia(clientID C.uint64_t, chatJID *C.char, path *C.char, mimeType *C.char, fileName *C.char, caption *C.char, contentType *C.char, replyID *C.char, replyParticipant *C.char, replyText *C.char) *C.char {
	mu.Lock()
	c, ok := clients[uint64(clientID)]
	mu.Unlock()
	if !ok {
		return cJSON(bridgeEvent{Type: "error", Message: "WhatsApp bridge client is not connected"})
	}

	chatRaw := C.GoString(chatJID)
	pathRaw := C.GoString(path)
	mimeRaw := firstNonEmpty(C.GoString(mimeType), mime.TypeByExtension(filepath.Ext(pathRaw)), "application/octet-stream")
	fileNameRaw := firstNonEmpty(C.GoString(fileName), filepath.Base(pathRaw), "upload")
	captionRaw := C.GoString(caption)
	replyIDRaw := C.GoString(replyID)
	replyParticipantRaw := C.GoString(replyParticipant)
	replyTextRaw := C.GoString(replyText)
	contentRaw := strings.ToLower(strings.TrimSpace(C.GoString(contentType)))
	contentRaw = normalizeOutboundContentType(contentRaw, mimeRaw)
	if contentRaw == "" {
		return cJSON(bridgeEvent{Type: "error", Message: "unsupported WhatsApp media type"})
	}

	info, err := os.Stat(pathRaw)
	if err != nil {
		return cJSON(bridgeEvent{Type: "error", Message: fmt.Sprintf("read WhatsApp media file: %v", err)})
	}
	if !info.Mode().IsRegular() {
		return cJSON(bridgeEvent{Type: "error", Message: fmt.Sprintf("WhatsApp media path is not a regular file: %s", pathRaw)})
	}

	c.log("sending WhatsApp media chat=%s path=%s file=%s mime=%s content_type=%s size=%d", chatRaw, pathRaw, fileNameRaw, mimeRaw, contentRaw, info.Size())

	if strings.HasPrefix(c.dbPath, "test:") {
		return cJSON(bridgeEvent{
			Type:           "sent",
			ID:             fmt.Sprintf("test-sent-media-%d", time.Now().UnixNano()),
			ChatJID:        chatRaw,
			SenderJID:      "test-device@s.whatsapp.net",
			Text:           captionRaw,
			Timestamp:      time.Now().UTC().Format(time.RFC3339Nano),
			FromMe:         true,
			IsGroup:        strings.Contains(chatRaw, "@g.us"),
			ContentType:    contentRaw,
			MediaID:        mediaID(pathRaw, fileNameRaw),
			MediaFileName:  fileNameRaw,
			MediaMime:      mimeRaw,
			MediaSize:      uint64(info.Size()),
			MediaLocalPath: pathRaw,
			Caption:        captionRaw,
		})
	}
	if c.wa == nil {
		return cJSON(bridgeEvent{Type: "error", Message: "WhatsApp bridge client is not connected"})
	}

	jid, err := types.ParseJID(chatRaw)
	if err != nil {
		return cJSON(bridgeEvent{Type: "error", Message: fmt.Sprintf("invalid WhatsApp chat JID: %v", err)})
	}
	dataPath, messageMime, messageContentType, cleanup, err := prepareOutboundMediaUpload(c, pathRaw, mimeRaw, contentRaw)
	if err != nil {
		return cJSON(bridgeEvent{Type: "error", Message: err.Error()})
	}
	if cleanup != nil {
		defer cleanup()
	}
	data, err := os.ReadFile(dataPath)
	if err != nil {
		return cJSON(bridgeEvent{Type: "error", Message: fmt.Sprintf("read WhatsApp media file: %v", err)})
	}
	if len(data) == 0 {
		return cJSON(bridgeEvent{Type: "error", Message: "cannot send an empty WhatsApp media file"})
	}

	appInfo := outboundMediaType(messageContentType)
	upload, err := c.wa.Upload(context.Background(), data, appInfo)
	if err != nil {
		c.log("upload WhatsApp media failed chat=%s path=%s mime=%s content_type=%s: %v", chatRaw, dataPath, messageMime, messageContentType, err)
		return cJSON(bridgeEvent{Type: "error", Message: fmt.Sprintf("upload WhatsApp media: %v", err)})
	}
	message := outboundMediaMessage(messageContentType, upload, messageMime, fileNameRaw, captionRaw)
	if message == nil {
		return cJSON(bridgeEvent{Type: "error", Message: "unsupported WhatsApp media type"})
	}
	if ctx := c.buildReplyContext(replyIDRaw, replyParticipantRaw, replyTextRaw); ctx != nil {
		attachContextInfo(message, ctx)
	}
	resp, err := c.wa.SendMessage(context.Background(), jid, message)
	if err != nil {
		c.log("send WhatsApp media failed chat=%s path=%s mime=%s content_type=%s: %v", chatRaw, dataPath, messageMime, messageContentType, err)
		return cJSON(bridgeEvent{Type: "error", Message: fmt.Sprintf("send WhatsApp media: %s", describeSendError(err))})
	}
	id := resp.ID
	if id == "" {
		id = string(whatsmeow.GenerateMessageID())
	}
	c.log("sent WhatsApp media id=%s chat=%s path=%s upload_path=%s mime=%s content_type=%s original_content_type=%s", id, jid.String(), pathRaw, dataPath, messageMime, messageContentType, contentRaw)
	canonicalChat := canonicalJID(c, context.Background(), jid)
	return cJSON(bridgeEvent{
		Type:           "sent",
		ID:             id,
		ChatJID:        canonicalChat.String(),
		SenderJID:      c.ownJID(),
		Text:           captionRaw,
		Timestamp:      time.Now().UTC().Format(time.RFC3339Nano),
		FromMe:         true,
		IsGroup:        strings.HasSuffix(jid.Server, "g.us"),
		ContentType:    contentRaw,
		MediaID:        mediaID(id, upload.DirectPath),
		MediaFileName:  fileNameRaw,
		MediaMime:      mimeRaw,
		MediaSize:      uint64(info.Size()),
		MediaLocalPath: pathRaw,
		Caption:        captionRaw,
	})
}

// C_EditMessage replaces the text of one of our own messages. The new body is
// built exactly like a fresh text send (so mentions survive) and wrapped with
// whatsmeow's BuildEdit. The result is an `edit` event naming the original
// message id; errors use the same `error` shape as C_SendText.
//
//export C_EditMessage
func C_EditMessage(clientID C.uint64_t, chatJID *C.char, messageID *C.char, text *C.char, mentionedJIDs *C.char) *C.char {
	mu.Lock()
	c, ok := clients[uint64(clientID)]
	mu.Unlock()
	if !ok {
		return cJSON(bridgeEvent{Type: "error", Message: "WhatsApp bridge client is not connected"})
	}

	chatRaw := C.GoString(chatJID)
	targetID := strings.TrimSpace(C.GoString(messageID))
	body := C.GoString(text)
	mentioned := parseMentionedJIDs(C.GoString(mentionedJIDs))
	if targetID == "" {
		return cJSON(bridgeEvent{Type: "error", Message: "cannot edit a WhatsApp message without its message ID"})
	}
	if strings.TrimSpace(body) == "" {
		return cJSON(bridgeEvent{Type: "error", Message: "cannot save an empty WhatsApp message"})
	}
	now := time.Now().UTC().Format(time.RFC3339Nano)
	if strings.HasPrefix(c.dbPath, "test:") {
		return cJSON(bridgeEvent{
			Type:      "edit",
			ID:        targetID,
			ChatJID:   chatRaw,
			SenderJID: "test-device@s.whatsapp.net",
			Text:      body,
			Timestamp: now,
			EditedAt:  now,
			FromMe:    true,
			IsGroup:   strings.Contains(chatRaw, "@g.us"),
		})
	}
	if c.wa == nil {
		return cJSON(bridgeEvent{Type: "error", Message: "WhatsApp bridge client is not connected"})
	}

	jid, err := types.ParseJID(chatRaw)
	if err != nil {
		return cJSON(bridgeEvent{Type: "error", Message: fmt.Sprintf("invalid WhatsApp chat JID: %v", err)})
	}

	content := buildTextMessage(c, body, "", "", "", mentioned)
	if _, err := c.wa.SendMessage(context.Background(), jid, c.wa.BuildEdit(jid, targetID, content)); err != nil {
		return cJSON(bridgeEvent{Type: "error", Message: fmt.Sprintf("edit WhatsApp message: %s", describeSendError(err))})
	}
	c.log("edited WhatsApp message id=%s chat=%s mentions=%d", targetID, jid.String(), len(mentioned))

	canonicalChat := canonicalJID(c, context.Background(), jid)
	return cJSON(bridgeEvent{
		Type:      "edit",
		ID:        targetID,
		ChatJID:   canonicalChat.String(),
		SenderJID: c.ownJID(),
		Text:      body,
		Timestamp: now,
		EditedAt:  now,
		FromMe:    true,
		IsGroup:   strings.HasSuffix(jid.Server, "g.us"),
	})
}

//export C_SendReaction
func C_SendReaction(clientID C.uint64_t, chatJID *C.char, senderJID *C.char, messageID *C.char, emoji *C.char) *C.char {
	mu.Lock()
	c, ok := clients[uint64(clientID)]
	mu.Unlock()
	if !ok {
		return cJSON(bridgeEvent{Type: "error", Message: "WhatsApp bridge client is not connected"})
	}

	chatRaw := C.GoString(chatJID)
	senderRaw := C.GoString(senderJID)
	targetID := C.GoString(messageID)
	reaction := C.GoString(emoji)
	if strings.HasPrefix(c.dbPath, "test:") {
		return cJSON(bridgeEvent{
			Type:                  "reaction",
			ID:                    fmt.Sprintf("test-reaction-%d", time.Now().UnixNano()),
			ChatJID:               chatRaw,
			SenderJID:             c.ownJID(),
			Timestamp:             time.Now().UTC().Format(time.RFC3339Nano),
			FromMe:                true,
			ReactionMessageID:     targetID,
			ReactionMessageFromMe: senderRaw == "" || senderRaw == c.ownJID(),
			ReactionEmoji:         reaction,
		})
	}
	if c.wa == nil {
		return cJSON(bridgeEvent{Type: "error", Message: "WhatsApp bridge client is not connected"})
	}
	if targetID == "" {
		return cJSON(bridgeEvent{Type: "error", Message: "cannot send WhatsApp reaction without a target message ID"})
	}

	chat, err := types.ParseJID(chatRaw)
	if err != nil {
		return cJSON(bridgeEvent{Type: "error", Message: fmt.Sprintf("invalid WhatsApp chat JID: %v", err)})
	}
	sender := types.EmptyJID
	if senderRaw != "" && senderRaw != "me" {
		if sender, err = types.ParseJID(senderRaw); err != nil {
			return cJSON(bridgeEvent{Type: "error", Message: fmt.Sprintf("invalid WhatsApp sender JID: %v", err)})
		}
	}

	resp, err := c.wa.SendMessage(context.Background(), chat, c.wa.BuildReaction(chat, sender, targetID, reaction))
	if err != nil {
		return cJSON(bridgeEvent{Type: "error", Message: fmt.Sprintf("send WhatsApp reaction: %s", describeSendError(err))})
	}
	id := resp.ID
	if id == "" {
		id = string(whatsmeow.GenerateMessageID())
	}
	canonicalChat := canonicalJID(c, context.Background(), chat)
	return cJSON(bridgeEvent{
		Type:                  "reaction",
		ID:                    id,
		ChatJID:               canonicalChat.String(),
		SenderJID:             c.ownJID(),
		Timestamp:             time.Now().UTC().Format(time.RFC3339Nano),
		FromMe:                true,
		ReactionMessageID:     targetID,
		ReactionMessageFromMe: sender.IsEmpty() || senderRaw == c.ownJID(),
		ReactionEmoji:         reaction,
	})
}

//export C_SendPollVote
func C_SendPollVote(clientID C.uint64_t, chatJID *C.char, senderJID *C.char, messageID *C.char, selectedOptionsJSON *C.char) *C.char {
	mu.Lock()
	c, ok := clients[uint64(clientID)]
	mu.Unlock()
	if !ok {
		return cJSON(bridgeEvent{Type: "error", Message: "WhatsApp bridge client is not connected"})
	}

	chatRaw := C.GoString(chatJID)
	senderRaw := C.GoString(senderJID)
	targetID := C.GoString(messageID)
	var selectedOptions []string
	if err := json.Unmarshal([]byte(C.GoString(selectedOptionsJSON)), &selectedOptions); err != nil {
		return cJSON(bridgeEvent{Type: "error", Message: fmt.Sprintf("invalid poll vote options: %v", err)})
	}
	if strings.HasPrefix(c.dbPath, "test:") {
		return cJSON(bridgeEvent{
			Type:              "poll_vote",
			ID:                fmt.Sprintf("test-poll-vote-%d", time.Now().UnixNano()),
			ChatJID:           chatRaw,
			SenderJID:         c.ownJID(),
			Timestamp:         time.Now().UTC().Format(time.RFC3339Nano),
			FromMe:            true,
			PollVoteMessageID: targetID,
			PollVoteOptions:   selectedOptions,
		})
	}
	if c.wa == nil {
		return cJSON(bridgeEvent{Type: "error", Message: "WhatsApp bridge client is not connected"})
	}
	if targetID == "" {
		return cJSON(bridgeEvent{Type: "error", Message: "cannot vote in a WhatsApp poll without a target message ID"})
	}

	chat, err := types.ParseJID(chatRaw)
	if err != nil {
		return cJSON(bridgeEvent{Type: "error", Message: fmt.Sprintf("invalid WhatsApp chat JID: %v", err)})
	}
	sender := types.EmptyJID
	if senderRaw != "" && senderRaw != "me" {
		if sender, err = types.ParseJID(senderRaw); err != nil {
			return cJSON(bridgeEvent{Type: "error", Message: fmt.Sprintf("invalid WhatsApp sender JID: %v", err)})
		}
	}
	pollInfo := &types.MessageInfo{
		MessageSource: types.MessageSource{
			Chat:     chat,
			Sender:   sender,
			IsFromMe: sender.IsEmpty() || senderRaw == c.ownJID(),
			IsGroup:  chat.Server == types.GroupServer,
		},
		ID: targetID,
	}
	pollVote, err := c.wa.BuildPollVote(context.Background(), pollInfo, selectedOptions)
	if err != nil {
		return cJSON(bridgeEvent{Type: "error", Message: fmt.Sprintf("build WhatsApp poll vote: %v", err)})
	}
	resp, err := c.wa.SendMessage(context.Background(), chat, pollVote)
	if err != nil {
		return cJSON(bridgeEvent{Type: "error", Message: fmt.Sprintf("send WhatsApp poll vote: %s", describeSendError(err))})
	}
	id := resp.ID
	if id == "" {
		id = string(whatsmeow.GenerateMessageID())
	}
	canonicalChat := canonicalJID(c, context.Background(), chat)
	return cJSON(bridgeEvent{
		Type:              "poll_vote",
		ID:                id,
		ChatJID:           canonicalChat.String(),
		SenderJID:         c.ownJID(),
		Timestamp:         time.Now().UTC().Format(time.RFC3339Nano),
		FromMe:            true,
		PollVoteMessageID: targetID,
		PollVoteOptions:   selectedOptions,
	})
}

//export C_SearchContacts
func C_SearchContacts(clientID C.uint64_t, query *C.char, limit C.int) *C.char {
	mu.Lock()
	c, ok := clients[uint64(clientID)]
	mu.Unlock()
	if !ok {
		return cJSON(bridgeEvent{Type: "error", Message: "unknown WhatsApp bridge client"})
	}

	queryText := strings.ToLower(strings.TrimSpace(C.GoString(query)))
	requested := int(limit)
	if queryText == "" || requested == 0 {
		return cJSON(bridgeEvent{Type: "contact_search"})
	}
	if requested < 0 || requested > 100 {
		requested = 100
	}

	if strings.HasPrefix(c.dbPath, "test:") {
		contacts := []bridgeContact{
			{JID: "447700900123@s.whatsapp.net", Name: "Alan Turing"},
			{JID: "447700900456@s.whatsapp.net", Name: "Katherine Johnson"},
			{JID: "447700900789@s.whatsapp.net", Name: "Grace Hopper"},
		}
		return cJSON(bridgeEvent{Type: "contact_search", Contacts: filterBridgeContacts(contacts, queryText, requested)})
	}
	if c.wa == nil || !c.wa.IsConnected() || c.wa.Store == nil || c.wa.Store.Contacts == nil {
		return cJSON(bridgeEvent{Type: "error", Message: "WhatsApp bridge client is not connected"})
	}

	started := time.Now()
	contacts, err := c.searchContacts(context.Background(), queryText, requested)
	if err != nil {
		c.log("search WhatsApp contacts failed query_len=%d limit=%d: %v", len(queryText), requested, err)
		return cJSON(bridgeEvent{Type: "error", Message: fmt.Sprintf("search WhatsApp contacts: %v", err)})
	}
	c.log("searched WhatsApp contacts query_len=%d limit=%d results=%d elapsed_ms=%d", len(queryText), requested, len(contacts), time.Since(started).Milliseconds())
	return cJSON(bridgeEvent{Type: "contact_search", Contacts: contacts})
}

// maxGroupMemberAvatars bounds how many participant profile pictures we resolve
// over the network for a single group-member listing so very large groups do
// not stall the request behind hundreds of sequential HTTP calls. Members past
// this limit are still listed (name + role); the UI falls back to initials.
const maxGroupMemberAvatars = 96

//export C_GroupMembers
func C_GroupMembers(clientID C.uint64_t, chatJID *C.char) *C.char {
	mu.Lock()
	c, ok := clients[uint64(clientID)]
	mu.Unlock()
	if !ok {
		return cJSON(bridgeEvent{Type: "error", Message: "unknown WhatsApp bridge client"})
	}

	chatRaw := C.GoString(chatJID)
	if strings.HasPrefix(c.dbPath, "test:") {
		return cJSON(bridgeEvent{Type: "group_members", ChatJID: chatRaw,
			GroupTopic:   "Holiday planning",
			GroupOwner:   "447700900123@s.whatsapp.net",
			GroupCreated: "2021-06-01T10:00:00Z",
			Members: []bridgeMember{
				{JID: "447700900123@s.whatsapp.net", Name: "Alan Turing", IsSuperAdmin: true},
				{JID: "447700900456@s.whatsapp.net", Name: "Katherine Johnson", IsAdmin: true},
				{JID: "447700900789@s.whatsapp.net", Name: "Grace Hopper"},
			}})
	}
	if c.wa == nil || !c.wa.IsConnected() {
		return cJSON(bridgeEvent{Type: "error", Message: "WhatsApp bridge client is not connected"})
	}

	jid, err := types.ParseJID(chatRaw)
	if err != nil || jid.IsEmpty() || jid.Server != types.GroupServer {
		return cJSON(bridgeEvent{Type: "error", Message: fmt.Sprintf("invalid WhatsApp group JID: %v", err)})
	}

	ctx := context.Background()
	started := time.Now()
	info, err := c.wa.GetGroupInfo(ctx, jid)
	if err != nil || info == nil {
		c.log("fetch WhatsApp group info failed chat=%s: %v", jid.String(), err)
		return cJSON(bridgeEvent{Type: "error", Message: fmt.Sprintf("fetch WhatsApp group info: %v", err)})
	}

	members := c.groupMembers(ctx, info)
	c.log("listed WhatsApp group members chat=%s members=%d elapsed_ms=%d", jid.String(), len(members), time.Since(started).Milliseconds())
	event := bridgeEvent{Type: "group_members", ChatJID: jid.String(), Members: members}
	event.GroupTopic = strings.TrimSpace(info.Topic)
	if !info.OwnerJID.IsEmpty() {
		event.GroupOwner = canonicalJIDString(c, ctx, info.OwnerJID)
	}
	if !info.GroupCreated.IsZero() {
		event.GroupCreated = info.GroupCreated.UTC().Format(time.RFC3339)
	}
	event.GroupOnlyAdminsSend = info.IsAnnounce
	event.GroupOnlyAdminsEdit = info.IsLocked
	if info.IsEphemeral {
		event.GroupDisappearingSeconds = info.DisappearingTimer
	}
	return cJSON(event)
}

// groupMembers resolves display names, avatars and admin roles for every
// participant of a group. Names come from the local contact store (with the
// participant push/display name as a fallback) and avatars are resolved
// best-effort up to maxGroupMemberAvatars.
func (c *client) groupMembers(ctx context.Context, info *types.GroupInfo) []bridgeMember {
	members := make([]bridgeMember, 0, len(info.Participants))
	avatarsResolved := 0
	for _, participant := range info.Participants {
		primary := participant.JID
		if primary.IsEmpty() {
			primary = participant.LID
		}
		if primary.IsEmpty() {
			continue
		}
		alternate := participant.PhoneNumber
		if alternate.IsEmpty() {
			alternate = participant.LID
		}

		name := participantName(c, ctx, primary, alternate, participant.DisplayName)
		if strings.TrimSpace(name) == "" {
			if !alternate.IsEmpty() {
				name = alternate.User
			} else {
				name = primary.User
			}
		}

		avatar := ""
		if avatarsResolved < maxGroupMemberAvatars {
			avatar = c.memberAvatarPath(ctx, primary, alternate)
			if avatar != "" {
				avatarsResolved++
			}
		}

		members = append(members, bridgeMember{
			JID:          canonicalJIDString(c, ctx, primary),
			Name:         name,
			AvatarPath:   avatar,
			IsAdmin:      participant.IsAdmin || participant.IsSuperAdmin,
			IsSuperAdmin: participant.IsSuperAdmin,
		})
	}

	sort.SliceStable(members, func(i, j int) bool {
		ri, rj := memberRank(members[i]), memberRank(members[j])
		if ri != rj {
			return ri < rj
		}
		return strings.ToLower(members[i].Name) < strings.ToLower(members[j].Name)
	})
	return members
}

// memberRank orders owners before admins before ordinary members so the most
// authoritative participants surface at the top of the list.
func memberRank(member bridgeMember) int {
	switch {
	case member.IsSuperAdmin:
		return 0
	case member.IsAdmin:
		return 1
	default:
		return 2
	}
}

// memberAvatarPath resolves a cached/downloaded profile-picture path for a
// group participant. It prefers the phone-number identity when the primary JID
// is a privacy @lid (those frequently have no directly fetchable picture).
func (c *client) memberAvatarPath(ctx context.Context, primary, alternate types.JID) string {
	if c == nil || c.wa == nil {
		return ""
	}
	target := primary
	if primary.Server == types.HiddenUserServer && !alternate.IsEmpty() {
		target = alternate
	}
	if target.IsEmpty() {
		return ""
	}
	info, err := c.wa.GetProfilePictureInfo(ctx, target, &whatsmeow.GetProfilePictureParams{Preview: true})
	if err != nil || info == nil || info.URL == "" {
		return ""
	}
	path, err := c.downloadProfilePicture(ctx, target.String(), info.ID, info.URL)
	if err != nil {
		return ""
	}
	return path
}

type markReadEntry struct {
	ID        string `json:"id"`
	SenderJID string `json:"sender_jid"`
}

//export C_MarkRead
func C_MarkRead(clientID C.uint64_t, chatJID *C.char, messagesJSON *C.char) *C.char {
	mu.Lock()
	c, ok := clients[uint64(clientID)]
	mu.Unlock()
	if !ok {
		return cJSON(bridgeEvent{Type: "error", Message: "unknown WhatsApp bridge client"})
	}

	chatRaw := C.GoString(chatJID)
	if strings.HasPrefix(c.dbPath, "test:") {
		return cJSON(bridgeEvent{Type: "read", ChatJID: chatRaw})
	}
	if c.wa == nil || !c.wa.IsConnected() {
		return cJSON(bridgeEvent{Type: "error", Message: "WhatsApp bridge client is not connected"})
	}

	chat, err := types.ParseJID(chatRaw)
	if err != nil || chat.IsEmpty() {
		return cJSON(bridgeEvent{Type: "error", Message: fmt.Sprintf("invalid WhatsApp chat JID: %v", err)})
	}
	var entries []markReadEntry
	if err := json.Unmarshal([]byte(C.GoString(messagesJSON)), &entries); err != nil {
		return cJSON(bridgeEvent{Type: "error", Message: fmt.Sprintf("invalid WhatsApp mark-read payload: %v", err)})
	}

	// Group receipts by sender: group chats require one receipt per
	// participant, while DMs use the chat JID itself.
	bySender := map[string][]types.MessageID{}
	senderOrder := []string{}
	for _, entry := range entries {
		if entry.ID == "" {
			continue
		}
		if _, seen := bySender[entry.SenderJID]; !seen {
			senderOrder = append(senderOrder, entry.SenderJID)
		}
		bySender[entry.SenderJID] = append(bySender[entry.SenderJID], types.MessageID(entry.ID))
	}
	if len(senderOrder) == 0 {
		return cJSON(bridgeEvent{Type: "read", ChatJID: chat.String()})
	}

	now := time.Now()
	marked := 0
	for _, senderRaw := range senderOrder {
		sender := chat
		if senderRaw != "" {
			if parsed, err := types.ParseJID(senderRaw); err == nil && !parsed.IsEmpty() {
				sender = parsed
			}
		}
		ids := bySender[senderRaw]
		if err := c.wa.MarkRead(context.Background(), ids, now, chat, sender); err != nil {
			c.log("mark WhatsApp chat read failed chat=%s sender=%s count=%d: %v", chat.String(), sender.String(), len(ids), err)
			return cJSON(bridgeEvent{Type: "error", Message: fmt.Sprintf("mark WhatsApp chat read: %v", err)})
		}
		marked += len(ids)
	}
	canonicalChat := canonicalJID(c, context.Background(), chat)
	c.log("marked WhatsApp chat read chat=%s messages=%d", chat.String(), marked)
	return cJSON(bridgeEvent{Type: "read", ChatJID: canonicalChat.String()})
}

//export C_FireSyntheticMessage
func C_FireSyntheticMessage(message *C.char) C.uint8_t {
	text := C.GoString(message)
	if strings.HasPrefix(strings.TrimSpace(text), "{") {
		emitRaw(text)
	} else {
		emit(bridgeEvent{
			Type:       "message",
			ID:         fmt.Sprintf("synthetic-%d", time.Now().UnixNano()),
			ChatJID:    "synthetic@s.whatsapp.net",
			SenderJID:  "synthetic@s.whatsapp.net",
			SenderName: "WhatsApp Test",
			Text:       text,
			Timestamp:  time.Now().UTC().Format(time.RFC3339Nano),
		})
	}
	return 1
}

//export C_FreeString
func C_FreeString(value *C.char) {
	if value != nil {
		C.free(unsafe.Pointer(value))
	}
}

//export C_Disconnect
func C_Disconnect(clientID C.uint64_t) {
	mu.Lock()
	c, ok := clients[uint64(clientID)]
	if ok {
		delete(clients, uint64(clientID))
	}
	mu.Unlock()

	if !ok {
		return
	}
	c.cancelCableSession()
	if c.cancel != nil {
		c.cancel()
	}
	if c.wa != nil {
		c.wa.Disconnect()
	}
	emit(bridgeEvent{Type: "disconnected"})
}

func (c *client) ownJID() string {
	if c.wa != nil && c.wa.Store.ID != nil {
		return c.wa.Store.ID.String()
	}
	return ""
}

// buildReplyContext constructs a WhatsApp ContextInfo that links an outgoing
// message to the message it replies to, so recipients render it as a native
// quoted reply. replyID is the quoted message's stanza ID; participant is the
// quoted sender's JID (empty or "me" resolves to our own JID); quotedText is a
// plain-text fallback shown when the recipient cannot resolve the original
// message locally. Returns nil when there is no reply target.
func (c *client) buildReplyContext(replyID, participant, quotedText string) *waProto.ContextInfo {
	replyID = strings.TrimSpace(replyID)
	if replyID == "" {
		return nil
	}
	ctx := &waProto.ContextInfo{StanzaID: proto.String(replyID)}

	participant = strings.TrimSpace(participant)
	if participant == "" || participant == "me" {
		participant = c.ownJID()
	}
	if participant != "" {
		if jid, err := types.ParseJID(participant); err == nil {
			ctx.Participant = proto.String(jid.String())
		}
	}

	ctx.QuotedMessage = &waProto.Message{Conversation: proto.String(quotedText)}
	return ctx
}

// parseMentionedJIDs splits the bridge's comma-separated mentioned-JID list
// (as passed from Rust) into individual JIDs, dropping empty entries. An empty
// input yields a nil slice, preserving the no-mention behavior.
func parseMentionedJIDs(raw string) []string {
	raw = strings.TrimSpace(raw)
	if raw == "" {
		return nil
	}
	parts := strings.Split(raw, ",")
	jids := make([]string, 0, len(parts))
	for _, part := range parts {
		if trimmed := strings.TrimSpace(part); trimmed != "" {
			jids = append(jids, trimmed)
		}
	}
	if len(jids) == 0 {
		return nil
	}
	return jids
}

// buildTextMessage returns the outgoing text message body, upgrading a plain
// conversation to an ExtendedTextMessage when a reply context or mention list
// is present so the quote and MentionedJID metadata survive to the recipient.
func buildTextMessage(c *client, body, replyID, participant, quotedText string, mentionedJIDs []string) *waProto.Message {
	ctx := c.buildReplyContext(replyID, participant, quotedText)
	if len(mentionedJIDs) > 0 {
		if ctx == nil {
			ctx = &waProto.ContextInfo{}
		}
		ctx.MentionedJID = mentionedJIDs
	}
	if ctx != nil {
		return &waProto.Message{ExtendedTextMessage: &waProto.ExtendedTextMessage{
			Text:        proto.String(body),
			ContextInfo: ctx,
		}}
	}
	return &waProto.Message{Conversation: proto.String(body)}
}

// attachContextInfo sets the reply ContextInfo on whichever media/text payload
// the outgoing message carries.
func attachContextInfo(message *waProto.Message, ctx *waProto.ContextInfo) {
	if message == nil || ctx == nil {
		return
	}
	switch {
	case message.ImageMessage != nil:
		message.ImageMessage.ContextInfo = ctx
	case message.VideoMessage != nil:
		message.VideoMessage.ContextInfo = ctx
	case message.AudioMessage != nil:
		message.AudioMessage.ContextInfo = ctx
	case message.DocumentMessage != nil:
		message.DocumentMessage.ContextInfo = ctx
	case message.StickerMessage != nil:
		message.StickerMessage.ContextInfo = ctx
	case message.ExtendedTextMessage != nil:
		message.ExtendedTextMessage.ContextInfo = ctx
	}
}

func boolPtr(value bool) *bool {
	return &value
}

func isMutedUntilActive(mutedUntil time.Time) bool {
	return mutedUntil == store.MutedForever || mutedUntil.After(time.Now())
}

type jidAlias struct {
	canonical types.JID
	alternate types.JID
}

func canonicalJID(c *client, ctx context.Context, jid types.JID) types.JID {
	alias := canonicalJIDAlias(c, ctx, jid)
	return alias.canonical
}

func canonicalJIDAlias(c *client, ctx context.Context, jid types.JID) jidAlias {
	if c == nil || c.wa == nil || c.wa.Store == nil || jid.IsEmpty() || jid.Server == types.GroupServer {
		return jidAlias{canonical: jid}
	}
	// Message-routing JIDs carry a device/agent suffix (e.g.
	// "40721274801:42@s.whatsapp.net") that contact, profile and group-member
	// JIDs never include. Strip it so a sender's identity matches the
	// device-less JID used everywhere else (avatars, names, member lists);
	// otherwise the same person shows up as two distinct identities and the
	// message header avatar can never be resolved from the member list.
	jid = jid.ToNonAD()
	alt, err := c.wa.Store.GetAltJID(ctx, jid)
	if err != nil || alt.IsEmpty() {
		return jidAlias{canonical: jid}
	}
	alt = alt.ToNonAD()
	canonical := chooseCanonicalJID(jid, alt)
	alternate := alt
	if canonical == alt {
		alternate = jid
	}
	return jidAlias{canonical: canonical, alternate: alternate}
}

func chooseCanonicalJID(jid, alt types.JID) types.JID {
	if jid.IsEmpty() || jid.Server == types.GroupServer || alt.IsEmpty() {
		return jid
	}
	if jid.Server == types.DefaultUserServer {
		return jid
	}
	if alt.Server == types.DefaultUserServer {
		return alt
	}
	return jid
}

func canonicalJIDString(c *client, ctx context.Context, jid types.JID) string {
	return canonicalJID(c, ctx, jid).String()
}

func chatMuted(c *client, ctx context.Context, jid types.JID) bool {
	if c == nil || c.wa == nil || c.wa.Store == nil || c.wa.Store.ChatSettings == nil || jid.IsEmpty() {
		return false
	}
	settings, err := c.wa.Store.ChatSettings.GetChatSettings(ctx, jid)
	if err != nil {
		c.log("load WhatsApp chat settings failed jid=%s: %v", jid.String(), err)
		return false
	}
	return isMutedUntilActive(settings.MutedUntil)
}

func (c *client) syncChatMuteSettings(ctx context.Context) {
	if c == nil || c.wa == nil || c.wa.Store == nil || c.wa.Store.ChatSettings == nil || strings.HasPrefix(c.dbPath, "test:") {
		return
	}
	if _, err := c.wa.DangerousInternals().FetchAppState(ctx, appstate.WAPatchRegularHigh, false, true); err != nil {
		c.log("sync WhatsApp mute app state failed: %v", err)
	}
}

func emitProfileEvent(c *client, ctx context.Context, jid types.JID, name string, avatarPath string, isGroup bool, muted *bool) {
	emitProfileEventWithActivity(c, ctx, jid, name, avatarPath, isGroup, muted, time.Time{}, "")
}

// emitProfileEventWithActivity emits a profile event optionally carrying
// chat-level last-message activity. The activity always describes an actual
// message (timestamp of the conversation's newest message, plus its preview
// text when the message payload is available), never synthetic conversation
// metadata, so the Rust side can use it for sidebar ordering.
func emitProfileEventWithActivity(c *client, ctx context.Context, jid types.JID, name string, avatarPath string, isGroup bool, muted *bool, lastMessageAt time.Time, lastMessagePreview string) {
	alias := canonicalJIDAlias(c, ctx, jid)
	event := bridgeEvent{Type: "profile", JID: alias.canonical.String(), SenderName: name, AvatarPath: avatarPath, IsGroup: isGroup, Muted: muted}
	if !lastMessageAt.IsZero() {
		event.LastMessageAt = lastMessageAt.UTC().Format(time.RFC3339Nano)
		event.LastMessagePreview = lastMessagePreview
	}
	if !alias.alternate.IsEmpty() && alias.alternate != alias.canonical {
		event.CanonicalJID = alias.canonical.String()
		event.AltJID = alias.alternate.String()
	}
	emit(event)
}

func emitMuteEvent(c *client, evt *events.Mute) {
	if evt == nil || evt.JID.IsEmpty() {
		return
	}
	muted := false
	if evt.Action != nil && evt.Action.GetMuted() {
		if evt.Action.GetMuteEndTimestamp() < 0 {
			muted = true
		} else {
			muted = time.UnixMilli(evt.Action.GetMuteEndTimestamp()).After(time.Now())
		}
	} else {
		muted = chatMuted(c, context.Background(), evt.JID)
	}
	emitProfileEvent(c, context.Background(), evt.JID, "", "", evt.JID.Server == types.GroupServer, &muted)
}

// emitSelfReadReceipt forwards read receipts produced by the user's other
// WhatsApp clients so this client can clear the chat's unread state. Receipts
// from other people reading our messages are ignored here.
func emitSelfReadReceipt(c *client, evt *events.Receipt) {
	if evt == nil || evt.Chat.IsEmpty() {
		return
	}
	isSelfRead := evt.Type == types.ReceiptTypeReadSelf ||
		(evt.Type == types.ReceiptTypeRead && evt.IsFromMe)
	if !isSelfRead {
		return
	}
	emitChatReadEvent(c, evt.Chat)
}

// emitMarkChatAsRead forwards app-state "chat marked read" actions performed
// on the user's other WhatsApp clients.
func emitMarkChatAsRead(c *client, evt *events.MarkChatAsRead) {
	if evt == nil || evt.JID.IsEmpty() || evt.Action == nil || !evt.Action.GetRead() {
		return
	}
	emitChatReadEvent(c, evt.JID)
}

func emitChatReadEvent(c *client, chatJID types.JID) {
	canonical := canonicalJID(c, context.Background(), chatJID)
	emit(bridgeEvent{Type: "read", ChatJID: canonical.String()})
}

// conversationUnreadCount resolves the unread badge a conversation should show
// from WhatsApp's authoritative history-sync metadata. It uses the reported
// UnreadCount, but also honors a manual "mark as unread" (MarkedAsUnread) even
// when no messages are genuinely unread, treating it as a single unread item so
// chats the user deliberately flagged on their phone stay flagged here too.
func conversationUnreadCount(conv *waHistorySync.Conversation) uint32 {
	if conv == nil {
		return 0
	}
	count := conv.GetUnreadCount()
	if count == 0 && conv.GetMarkedAsUnread() {
		return 1
	}
	return count
}

// emitChatUnread reports the authoritative unread count for a conversation as
// tracked by WhatsApp itself (history-sync Conversation.UnreadCount). Unlike
// live/offline message arrivals, which the consumer accumulates locally, this
// is the phone's source-of-truth read state and is allowed to lower an
// inflated count (e.g. after an offline reconnect replays messages the user
// had already read on their phone).
func emitChatUnread(c *client, ctx context.Context, chatJID types.JID, count uint32) {
	canonical := canonicalJID(c, ctx, chatJID)
	unread := count
	emit(bridgeEvent{Type: "chat_unread", ChatJID: canonical.String(), UnreadCount: &unread})
}

func handleWhatsAppEvent(c *client, evt interface{}) {
	switch v := evt.(type) {
	case *events.Message:
		// During offline-sync replay, tag the message as backlog so the
		// consumer suppresses notifications while still recording history and
		// unread counts. Live messages (steady-state) use "message".
		eventType := "message"
		if c != nil && c.offlineSync.Load() {
			eventType = "offline"
		}
		emitMessageEvent(c, v, eventType)
	case *events.OfflineSyncPreview:
		if c != nil {
			c.offlineSync.Store(true)
			c.log("offline sync started: replaying %d missed messages", v.Messages)
		}
	case *events.OfflineSyncCompleted:
		if c != nil {
			c.offlineSync.Store(false)
			c.log("offline sync completed after %d events", v.Count)
		}
	case *events.Mute:
		emitMuteEvent(c, v)
	case *events.HistorySync:
		emitHistorySync(c, v)
	case *events.PushName:
		emitProfileEvent(c, context.Background(), v.JID, v.NewPushName, "", false, nil)
	case *events.Picture:
		if v.Remove {
			ctx := context.Background()
			muted := chatMuted(c, ctx, v.JID)
			emitProfileEvent(c, ctx, v.JID, "", "", v.JID.Server == types.GroupServer, &muted)
		} else {
			go c.fetchAndEmitProfile(context.Background(), v.JID, "", v.JID.Server == types.GroupServer)
		}
	case *events.GroupInfo:
		if v.Name != nil && v.Name.Name != "" {
			muted := chatMuted(c, context.Background(), v.JID)
			emitProfileEvent(c, context.Background(), v.JID, v.Name.Name, "", true, &muted)
		}
	case *events.Receipt:
		emitSelfReadReceipt(c, v)
	case *events.MarkChatAsRead:
		emitMarkChatAsRead(c, v)
	case *events.PairPasskeyConfirmation:
		// Surface the pairing verification code so the user can confirm it
		// matches the code shown on their phone. whatsmeow dispatches this
		// event to every registered handler, so it fires even on the
		// SkipHandoffUX auto-confirm path, where the QR channel consumes it
		// internally (qrchan.go) and never forwards it to the QR loop — which
		// is why the code was previously invisible in chat-cli. Display only:
		// the actual SendPasskeyConfirmation is performed by the QR channel
		// (auto-confirm) or by handlePasskeyConfirmation (manual path).
		if v != nil {
			c.log("passkey confirmation code=%s skipHandoffUX=%t", v.Code, v.SkipHandoffUX)
			emit(bridgeEvent{Type: "login", Event: "passkey-confirmation", Code: v.Code})
		}
	case *events.Connected:
		// Announce availability like the official web client does right
		// after login. Without this the companion never sends <presence/>,
		// so the primary phone sees a device that authenticated but never
		// "showed up" — a plausible input to its decision to purge the
		// device later (observed as device_removed hours after pairing).
		// It is also required for the phone to display last-active state
		// in the Linked Devices list. Side effect (same as WhatsApp Web
		// being open): contacts may see the account as online while
		// chat-cli runs; CHATCLI_WHATSAPP_NO_PRESENCE opts out.
		if c != nil && c.wa != nil && os.Getenv("CHATCLI_WHATSAPP_NO_PRESENCE") == "" {
			go func() {
				if err := c.wa.SendPresence(context.Background(), types.PresenceAvailable); err != nil {
					c.log("send available presence failed: %v", err)
				} else {
					c.log("announced available presence")
				}
			}()
		}
		emit(bridgeEvent{Type: "connected"})
	case *events.LoggedOut:
		// A revoked session makes any in-flight caBLE ceremony moot.
		if c != nil {
			c.cancelCableSession()
		}
		emit(bridgeEvent{Type: "disconnected", Reason: "logged out"})
	case *events.Disconnected:
		emit(bridgeEvent{Type: "disconnected"})
	case *events.StreamReplaced:
		emit(bridgeEvent{Type: "disconnected", Reason: "stream replaced"})
	default:
		// Diagnostic only (gated behind the same env as whatsmeow logging):
		// surface event types the bridge ignores. The phone-side "Logging
		// in…" hang after passkey pairing suggests the server may push a
		// finalization step we silently drop; this makes such a push visible.
		if c != nil && os.Getenv("CHATCLI_WHATSAPP_LOG") != "" {
			c.log("unhandled whatsmeow event %T", evt)
		}
	}
}

func emitHistorySync(c *client, evt *events.HistorySync) {
	if evt == nil || evt.Data == nil || c == nil || c.wa == nil {
		return
	}
	isOnDemand := evt.Data.GetSyncType() == waHistorySync.HistorySync_ON_DEMAND
	if c.syncScope == "none" && !isOnDemand {
		c.log("skipping WhatsApp history sync because sync_scope=none")
		emit(bridgeEvent{Type: "sync", Progress: 100})
		return
	}

	progress := evt.Data.GetProgress()
	if progress > 100 {
		progress = 100
	}
	emit(bridgeEvent{Type: "sync", Progress: uint8(progress)})

	ctx := context.Background()
	emittedMessages := 0
	for _, pushName := range evt.Data.GetPushnames() {
		jid, err := types.ParseJID(pushName.GetID())
		if err == nil {
			emitProfileEvent(c, ctx, jid, pushName.GetPushname(), "", false, nil)
		}
	}

	for _, conv := range evt.Data.GetConversations() {
		chatJID, err := types.ParseJID(conv.GetID())
		if err != nil || chatJID.IsEmpty() {
			continue
		}
		isGroup := chatJID.Server == types.GroupServer
		chatName := conversationName(c, ctx, chatJID, firstNonEmpty(conv.GetDisplayName(), conv.GetName()))
		chatMuted := chatMuted(c, ctx, chatJID)

		parsedMessages := make([]*events.Message, 0, len(conv.GetMessages()))
		for _, historyMsg := range conv.GetMessages() {
			webMessage := historyMsg.GetMessage()
			if webMessage == nil {
				continue
			}
			message, err := c.wa.ParseWebMessage(chatJID, webMessage)
			if err != nil {
				continue
			}
			parsedMessages = append(parsedMessages, message)
		}

		// Carry the conversation's real last-message activity even when the
		// sync scope skips replaying the messages themselves, so recently
		// contacted chats never show up as "No messages yet".
		lastMessageAt, lastMessagePreview := conversationActivity(c, ctx, parsedMessages, conv.GetLastMsgTimestamp())
		emitProfileEventWithActivity(c, ctx, chatJID, chatName, "", isGroup, &chatMuted, lastMessageAt, lastMessagePreview)
		// Report the phone's authoritative unread count so the consumer can
		// reconcile any locally-accumulated count (e.g. offline-backlog replay
		// that re-counted messages the user had already read on their phone).
		emitChatUnread(c, ctx, chatJID, conversationUnreadCount(conv))
		go c.fetchAndEmitProfile(ctx, chatJID, chatName, isGroup)

		for _, message := range parsedMessages {
			if !isOnDemand && c.shouldSkipHistoryMessage(message.Info.Timestamp) {
				continue
			}
			emittedMessages++
			emitMessageEvent(c, message, "history")
		}
	}

	c.log("processed WhatsApp history sync progress=%d emitted_messages=%d sync_scope=%s", progress, emittedMessages, c.syncScope)
	emit(bridgeEvent{Type: "sync", Progress: 100})
}

func displayableMessage(message *waProto.Message) *waProto.Message {
	for message != nil {
		switch {
		case message.GetEphemeralMessage() != nil:
			message = message.GetEphemeralMessage().GetMessage()
		case message.GetViewOnceMessage() != nil:
			message = message.GetViewOnceMessage().GetMessage()
		case message.GetViewOnceMessageV2() != nil:
			message = message.GetViewOnceMessageV2().GetMessage()
		case message.GetViewOnceMessageV2Extension() != nil:
			message = message.GetViewOnceMessageV2Extension().GetMessage()
		case message.GetDocumentWithCaptionMessage() != nil:
			message = message.GetDocumentWithCaptionMessage().GetMessage()
		case message.GetLottieStickerMessage() != nil:
			message = message.GetLottieStickerMessage().GetMessage()
		case message.GetDeviceSentMessage() != nil:
			message = message.GetDeviceSentMessage().GetMessage()
		case message.GetEditedMessage() != nil:
			message = message.GetEditedMessage().GetMessage()
		default:
			return message
		}
	}
	return nil
}

func emitMessageEvent(c *client, message *events.Message, eventType string) {
	if message == nil {
		return
	}
	messagePayload := displayableMessage(message.Message)
	if messagePayload == nil {
		return
	}
	ctx := context.Background()
	chatJID := message.Info.Chat
	if message.Info.IsIncomingBroadcast() {
		chatJID = message.Info.Sender
	}
	isGroup := message.Info.IsGroup
	chatName := ""
	if c != nil {
		chatName = conversationName(c, ctx, chatJID, "")
	}
	if chatName == "" {
		chatName = message.Info.PushName
	}

	if chatName == "" && isGroup && c != nil && c.wa != nil {
		go c.fetchAndEmitGroupInfo(ctx, chatJID)
	}

	senderName := message.Info.PushName
	if c != nil && !message.Info.Sender.IsEmpty() && !message.Info.IsFromMe {
		senderName = participantName(c, ctx, message.Info.Sender, message.Info.SenderAlt, senderName)
	}

	ownJID := ""
	if c != nil {
		ownJID = c.ownJID()
	}
	mentionsMe := !message.Info.IsFromMe && messageMentionsUser(messagePayload, ownJID)

	// Edits must be recognised before anything else: live edits arrive as a
	// MESSAGE_EDIT protocol message (previously dropped as unsupported) and
	// history-synced edits are pre-rewritten by ParseWebMessage into a normal
	// looking message stamped with the edit time (previously duplicated).
	if edit, ok := detectMessageEdit(message); ok {
		if event, ok := buildEditEvent(c, ctx, message, edit, chatJID, chatName, senderName, isGroup); ok {
			emit(event)
		}
		return
	}

	if reaction := messagePayload.GetReactionMessage(); reaction != nil {
		emitReactionMessageEvent(c, message, reaction, chatJID, chatName, senderName, isGroup)
		return
	}
	if messagePayload.GetEncReactionMessage() != nil {
		if c != nil && c.wa != nil {
			reaction, err := c.wa.DecryptReaction(ctx, message)
			if err == nil && reaction != nil {
				emitReactionMessageEvent(c, message, reaction, chatJID, chatName, senderName, isGroup)
			}
		}
		return
	}
	if messagePayload.GetPollUpdateMessage() != nil {
		if c != nil && c.wa != nil {
			vote, err := c.wa.DecryptPollVote(ctx, message)
			if err == nil && vote != nil {
				emitPollVoteEvent(c, message, vote, chatJID, chatName, senderName, isGroup)
			}
		}
		return
	}

	canonicalChatAlias := canonicalJIDAlias(c, ctx, chatJID)
	canonicalSenderJID := canonicalJID(c, ctx, message.Info.Sender)

	event := bridgeEvent{
		Type:       eventType,
		ID:         message.Info.ID,
		ChatJID:    canonicalChatAlias.canonical.String(),
		ChatName:   chatName,
		SenderJID:  canonicalSenderJID.String(),
		SenderName: senderName,
		Text:       resolveMentions(c, ctx, messageText(messagePayload), messageMentionedJID(messagePayload)),
		Timestamp:  message.Info.Timestamp.UTC().Format(time.RFC3339Nano),
		FromMe:     message.Info.IsFromMe,
		MentionsMe: mentionsMe,
		IsGroup:    isGroup,
		Muted:      boolPtr(chatMuted(c, ctx, chatJID)),
		Reactions:  messageReactions(c, ctx, message),
	}
	if !canonicalChatAlias.alternate.IsEmpty() && canonicalChatAlias.alternate != canonicalChatAlias.canonical {
		event.CanonicalJID = canonicalChatAlias.canonical.String()
		event.AltJID = canonicalChatAlias.alternate.String()
	}
	if poll := pollCreation(messagePayload); poll != nil {
		event.ContentType = "poll"
		event.PollQuestion = poll.GetName()
		event.Text = poll.GetName()
		event.PollSelectable = poll.GetSelectableOptionsCount()
		for _, option := range poll.GetOptions() {
			if name := option.GetOptionName(); name != "" {
				event.PollOptions = append(event.PollOptions, name)
				hash := sha256.Sum256([]byte(name))
				event.PollOptionIDs = append(event.PollOptionIDs, hex.EncodeToString(hash[:]))
			}
		}
	} else {
		applyMedia(c, message.Info.ID, messagePayload, &event)
	}
	if shouldSkipUnsupportedDisplayMessage(event) {
		return
	}
	emit(event)

	if c != nil {
		ctx := context.Background()
		profileTarget := chatJID
		profileName := chatName
		if !isGroup && !message.Info.Sender.IsEmpty() && !message.Info.IsFromMe {
			profileTarget = message.Info.Sender
			profileName = senderName
		}
		go c.fetchAndEmitProfile(ctx, profileTarget, profileName, isGroup)
		if isGroup && !message.Info.Sender.IsEmpty() {
			go c.fetchAndEmitProfile(ctx, message.Info.Sender, senderName, false)
		}
		if !message.Info.SenderAlt.IsEmpty() {
			go c.fetchAndEmitProfile(ctx, message.Info.SenderAlt, senderName, false)
		}
	}
}

// messageEdit describes an edit found in an incoming WhatsApp message.
type messageEdit struct {
	targetID string
	content  *waProto.Message
	editedAt time.Time
}

// detectMessageEdit recognises both edit shapes whatsmeow produces:
//   - live: Message is a MESSAGE_EDIT ProtocolMessage whose Key names the
//     target and whose EditedMessage carries the new content;
//   - history: ParseWebMessage has already replaced Info.ID with the target
//     and Message with the new content, so the raw payload is what still
//     identifies it as an edit.
//
// Admin revokes share the EditedMessage wrapper, so the protocol type is
// always checked rather than relying on IsEdit alone.
func detectMessageEdit(message *events.Message) (messageEdit, bool) {
	if message == nil {
		return messageEdit{}, false
	}
	if protocol := displayableMessage(message.Message).GetProtocolMessage(); protocol.GetType() == waProto.ProtocolMessage_MESSAGE_EDIT {
		targetID := protocol.GetKey().GetID()
		if targetID == "" || protocol.GetEditedMessage() == nil {
			return messageEdit{}, false
		}
		return messageEdit{
			targetID: targetID,
			content:  protocol.GetEditedMessage(),
			editedAt: editTimestamp(protocol, message.Info.Timestamp),
		}, true
	}
	if raw := displayableMessage(message.RawMessage).GetProtocolMessage(); raw.GetType() == waProto.ProtocolMessage_MESSAGE_EDIT {
		if message.Info.ID == "" || message.Message == nil {
			return messageEdit{}, false
		}
		return messageEdit{
			targetID: message.Info.ID,
			content:  message.Message,
			editedAt: editTimestamp(raw, message.Info.Timestamp),
		}, true
	}
	return messageEdit{}, false
}

func editTimestamp(protocol *waProto.ProtocolMessage, fallback time.Time) time.Time {
	if ms := protocol.GetTimestampMS(); ms > 0 {
		return time.UnixMilli(ms)
	}
	return fallback
}

// editedText returns the user-visible text of edited content. Only text and
// captions can be edited on WhatsApp; anything else is ignored.
func editedText(content *waProto.Message) (string, bool) {
	content = displayableMessage(content)
	switch {
	case content == nil:
		return "", false
	case content.Conversation != nil:
		return content.GetConversation(), true
	case content.GetExtendedTextMessage() != nil:
		return content.GetExtendedTextMessage().GetText(), true
	case content.GetImageMessage() != nil:
		return content.GetImageMessage().GetCaption(), true
	case content.GetVideoMessage() != nil:
		return content.GetVideoMessage().GetCaption(), true
	case content.GetDocumentMessage() != nil:
		return content.GetDocumentMessage().GetCaption(), true
	}
	return "", false
}

func buildEditEvent(c *client, ctx context.Context, message *events.Message, edit messageEdit, chatJID types.JID, chatName, senderName string, isGroup bool) (bridgeEvent, bool) {
	text, ok := editedText(edit.content)
	if !ok {
		return bridgeEvent{}, false
	}
	return bridgeEvent{
		Type:       "edit",
		ID:         edit.targetID,
		ChatJID:    canonicalJIDString(c, ctx, chatJID),
		ChatName:   chatName,
		SenderJID:  canonicalJIDString(c, ctx, message.Info.Sender),
		SenderName: senderName,
		Text:       resolveMentions(c, ctx, text, messageMentionedJID(edit.content)),
		Timestamp:  message.Info.Timestamp.UTC().Format(time.RFC3339Nano),
		EditedAt:   edit.editedAt.UTC().Format(time.RFC3339Nano),
		FromMe:     message.Info.IsFromMe,
		IsGroup:    isGroup,
	}, true
}

func emitReactionMessageEvent(c *client, message *events.Message, reaction *waProto.ReactionMessage, chatJID types.JID, chatName, senderName string, isGroup bool) {
	if message == nil || reaction == nil {
		return
	}
	targetKey := reaction.GetKey()
	targetID := targetKey.GetID()
	if targetID == "" {
		return
	}
	emit(bridgeEvent{
		Type:                  "reaction",
		ID:                    message.Info.ID,
		ChatJID:               canonicalJIDString(c, context.Background(), chatJID),
		ChatName:              chatName,
		SenderJID:             canonicalJIDString(c, context.Background(), message.Info.Sender),
		SenderName:            senderName,
		Timestamp:             message.Info.Timestamp.UTC().Format(time.RFC3339Nano),
		FromMe:                message.Info.IsFromMe,
		IsGroup:               isGroup,
		ReactionMessageID:     targetID,
		ReactionMessageFromMe: targetKey.GetFromMe(),
		ReactionEmoji:         reaction.GetText(),
	})
}

func emitPollVoteEvent(c *client, message *events.Message, vote *waProto.PollVoteMessage, chatJID types.JID, chatName, senderName string, isGroup bool) {
	if message == nil || vote == nil {
		return
	}
	pollUpdate := message.Message.GetPollUpdateMessage()
	if pollUpdate == nil || pollUpdate.GetPollCreationMessageKey().GetID() == "" {
		return
	}
	selected := make([]string, 0, len(vote.GetSelectedOptions()))
	for _, optionHash := range vote.GetSelectedOptions() {
		if len(optionHash) > 0 {
			selected = append(selected, hex.EncodeToString(optionHash))
		}
	}
	emit(bridgeEvent{
		Type:              "poll_vote",
		ID:                message.Info.ID,
		ChatJID:           canonicalJIDString(c, context.Background(), chatJID),
		ChatName:          chatName,
		SenderJID:         canonicalJIDString(c, context.Background(), message.Info.Sender),
		SenderName:        senderName,
		Timestamp:         message.Info.Timestamp.UTC().Format(time.RFC3339Nano),
		FromMe:            message.Info.IsFromMe,
		IsGroup:           isGroup,
		PollVoteMessageID: pollUpdate.GetPollCreationMessageKey().GetID(),
		PollVoteOptions:   selected,
	})
}

func messageReactions(c *client, ctx context.Context, message *events.Message) []bridgeReaction {
	if message == nil || message.SourceWebMsg == nil {
		return nil
	}
	reactions := message.SourceWebMsg.GetReactions()
	if len(reactions) == 0 {
		return nil
	}

	byEmoji := make(map[string]map[string]struct{})
	for _, reaction := range reactions {
		if reaction == nil {
			continue
		}
		emoji := reaction.GetText()
		if emoji == "" {
			continue
		}
		sender := reactionSenderID(reaction.GetKey())
		if sender == "" {
			sender = message.Info.Sender.String()
		}
		// History-synced reactions carry the raw participant JID, which can be
		// a LID or device-suffixed JID. Canonicalize it to the same device-less
		// identity used for message senders and group members so the reactor
		// resolves to a contact name instead of a raw phone number/JID.
		if parsed, err := types.ParseJID(sender); err == nil && !parsed.IsEmpty() {
			sender = canonicalJIDString(c, ctx, parsed)
		}
		if byEmoji[emoji] == nil {
			byEmoji[emoji] = make(map[string]struct{})
		}
		byEmoji[emoji][sender] = struct{}{}
	}
	if len(byEmoji) == 0 {
		return nil
	}

	emojis := make([]string, 0, len(byEmoji))
	for emoji := range byEmoji {
		emojis = append(emojis, emoji)
	}
	sort.Strings(emojis)

	result := make([]bridgeReaction, 0, len(emojis))
	for _, emoji := range emojis {
		senders := make([]string, 0, len(byEmoji[emoji]))
		for sender := range byEmoji[emoji] {
			senders = append(senders, sender)
		}
		sort.Strings(senders)
		result = append(result, bridgeReaction{Emoji: emoji, Sender: senders})
	}
	return result
}

func reactionSenderID(key *waProto.MessageKey) string {
	if key == nil {
		return ""
	}
	if participant := key.GetParticipant(); participant != "" {
		return participant
	}
	return key.GetRemoteJID()
}

func conversationName(c *client, ctx context.Context, jid types.JID, fallback string) string {
	if jid.Server == types.GroupServer && c != nil && c.wa != nil {
		if info, err := c.wa.GetGroupInfo(ctx, jid); err == nil && info != nil && info.Name != "" {
			return info.Name
		}
	}
	if name, ok := contactDisplayName(c, ctx, jid); ok {
		return name
	}
	if fallback != "" {
		return fallback
	}
	return jid.User
}

func participantName(c *client, ctx context.Context, primary, alternate types.JID, fallback string) string {
	if name, ok := contactDisplayName(c, ctx, primary); ok {
		return name
	}
	if name, ok := contactDisplayName(c, ctx, alternate); ok {
		return name
	}
	if fallback != "" {
		return fallback
	}
	if !primary.IsEmpty() {
		return primary.User
	}
	return ""
}

func contactDisplayName(c *client, ctx context.Context, jid types.JID) (string, bool) {
	if c == nil || c.wa == nil || c.wa.Store == nil || c.wa.Store.Contacts == nil || jid.IsEmpty() {
		return "", false
	}
	if name, ok := contactDisplayNameForJID(c, ctx, jid); ok {
		return name, true
	}
	for _, alt := range alternateContactJIDs(c, ctx, jid) {
		if name, ok := contactDisplayNameForJID(c, ctx, alt); ok {
			return name, true
		}
	}
	return "", false
}

// alternateContactJIDs returns the other JIDs a contact may be stored under.
// A group sender is frequently addressed by a privacy @lid that has no contact
// entry of its own, while the saved address-book contact lives under the
// phone-number JID (and vice versa). We consult both the generic alt-JID record
// and the dedicated LID<->PN mapping store so the contact resolves regardless of
// which identity the message carried.
func alternateContactJIDs(c *client, ctx context.Context, jid types.JID) []types.JID {
	if c == nil || c.wa == nil || c.wa.Store == nil || jid.IsEmpty() {
		return nil
	}
	seen := map[string]struct{}{jid.String(): {}}
	var alts []types.JID
	add := func(candidate types.JID, err error) {
		if err != nil || candidate.IsEmpty() {
			return
		}
		key := candidate.String()
		if _, ok := seen[key]; ok {
			return
		}
		seen[key] = struct{}{}
		alts = append(alts, candidate)
	}

	alt, err := c.wa.Store.GetAltJID(ctx, jid)
	add(alt, err)

	if c.wa.Store.LIDs != nil {
		switch jid.Server {
		case types.HiddenUserServer:
			pn, err := c.wa.Store.LIDs.GetPNForLID(ctx, jid)
			add(pn, err)
		case types.DefaultUserServer:
			lid, err := c.wa.Store.LIDs.GetLIDForPN(ctx, jid)
			add(lid, err)
		}
	}
	return alts
}

func contactDisplayNameForJID(c *client, ctx context.Context, jid types.JID) (string, bool) {
	if contact, err := c.wa.Store.Contacts.GetContact(ctx, jid); err == nil {
		if name := contactRealName(contact); name != "" {
			return name, true
		}
	}
	if jid.Server == types.DefaultUserServer && c.wa != nil {
		if info, err := c.wa.IsOnWhatsApp(ctx, []string{jid.User}); err == nil {
			for _, item := range info {
				if item.JID == jid && item.VerifiedName != nil {
					if name := strings.TrimSpace(item.VerifiedName.Details.GetVerifiedName()); name != "" {
						return name, true
					}
				}
			}
		}
	}
	return "", false
}

// contactRealName returns the first genuine name stored for a contact. It never
// falls back to the bare JID user (phone number / LID) so callers can tell a
// real resolution apart from "no name known". This is critical for the resolver
// chain in participantName/conversationName: treating the bare number as a
// successful resolution would short-circuit before trying the alternate JID
// (e.g. the phone number behind a @lid sender) or the message push name.
func contactRealName(contact types.ContactInfo) string {
	for _, name := range []string{contact.FullName, contact.FirstName, contact.BusinessName, contact.PushName} {
		if strings.TrimSpace(name) != "" {
			return name
		}
	}
	return ""
}

func displayNameForContact(contact types.ContactInfo, jid types.JID) string {
	if name := contactRealName(contact); name != "" {
		return name
	}
	return jid.User
}

// addressBookContactName returns the saved address-book name for a contact.
// Push names are deliberately excluded so only contacts the user actually
// saved are surfaced as sidebar chats; push-name-only senders already get
// chats through the messages they send.
func addressBookContactName(contact types.ContactInfo) string {
	for _, name := range []string{contact.FullName, contact.FirstName, contact.BusinessName} {
		if trimmed := strings.TrimSpace(name); trimmed != "" {
			return trimmed
		}
	}
	return ""
}

// emitContacts surfaces every saved address-book contact as a chat (via the
// existing "profile" event) so the sidebar's older-chats fold also lists
// people the user never exchanged a message with.
func (c *client) emitContacts(ctx context.Context) {
	if c == nil || c.wa == nil || strings.HasPrefix(c.dbPath, "test:") {
		return
	}
	if c.wa.Store == nil || c.wa.Store.Contacts == nil {
		return
	}
	storedContacts, err := c.wa.Store.Contacts.GetAllContacts(ctx)
	if err != nil {
		c.log("fetch WhatsApp contacts failed: %v", err)
		return
	}
	emitted := 0
	for jid, contact := range storedContacts {
		if jid.IsEmpty() || (jid.Server != types.DefaultUserServer && jid.Server != types.HiddenUserServer) {
			continue
		}
		name := addressBookContactName(contact)
		if name == "" {
			continue
		}
		emitProfileEvent(c, ctx, jid, name, "", false, nil)
		emitted++
	}
	c.log("emitted WhatsApp contacts count=%d stored=%d", emitted, len(storedContacts))
}

func (c *client) searchContacts(ctx context.Context, query string, limit int) ([]bridgeContact, error) {
	if c == nil || c.wa == nil || c.wa.Store == nil || c.wa.Store.Contacts == nil {
		return nil, fmt.Errorf("contact store unavailable")
	}
	storedContacts, err := c.wa.Store.Contacts.GetAllContacts(ctx)
	if err != nil {
		return nil, err
	}
	jids := make([]types.JID, 0, len(storedContacts))
	for jid := range storedContacts {
		if jid.IsEmpty() || jid.Server == types.GroupServer {
			continue
		}
		jids = append(jids, jid)
	}
	sort.Slice(jids, func(i, j int) bool {
		left := strings.ToLower(displayNameForContact(storedContacts[jids[i]], jids[i]))
		right := strings.ToLower(displayNameForContact(storedContacts[jids[j]], jids[j]))
		if left == right {
			return jids[i].String() < jids[j].String()
		}
		return left < right
	})

	results := make([]bridgeContact, 0, limit)
	seen := make(map[string]struct{})
	for _, jid := range jids {
		contact := storedContacts[jid]
		name := displayNameForContact(contact, jid)
		canonical := canonicalJID(c, ctx, jid)
		if !contactMatchesQuery(contact, jid, canonical, name, query) {
			continue
		}
		canonicalText := canonical.String()
		if _, ok := seen[canonicalText]; ok {
			continue
		}
		seen[canonicalText] = struct{}{}
		results = append(results, bridgeContact{JID: canonicalText, Name: name})
		if len(results) >= limit {
			break
		}
	}
	return results, nil
}

func contactMatchesQuery(contact types.ContactInfo, jid, canonical types.JID, displayName, query string) bool {
	if query == "" {
		return false
	}
	candidates := []string{
		displayName,
		contact.FullName,
		contact.FirstName,
		contact.BusinessName,
		contact.PushName,
		contact.RedactedPhone,
		jid.String(),
		jid.User,
		canonical.String(),
		canonical.User,
	}
	for _, candidate := range candidates {
		if strings.Contains(strings.ToLower(candidate), query) {
			return true
		}
	}
	return false
}

func filterBridgeContacts(contacts []bridgeContact, query string, limit int) []bridgeContact {
	results := make([]bridgeContact, 0, limit)
	for _, contact := range contacts {
		if strings.Contains(strings.ToLower(contact.Name), query) || strings.Contains(strings.ToLower(contact.JID), query) {
			results = append(results, contact)
			if len(results) >= limit {
				break
			}
		}
	}
	return results
}

func (c *client) fetchAndEmitGroupInfo(ctx context.Context, jid types.JID) {
	if c == nil || c.wa == nil || jid.IsEmpty() || jid.Server != types.GroupServer || strings.HasPrefix(c.dbPath, "test:") {
		return
	}
	info, err := c.wa.GetGroupInfo(ctx, jid)
	if err == nil && info != nil && info.Name != "" {
		muted := chatMuted(c, ctx, jid)
		emit(bridgeEvent{Type: "profile", JID: jid.String(), SenderName: info.Name, IsGroup: true, Muted: &muted})
	}
}

func (c *client) emitJoinedGroups(ctx context.Context) {
	if c == nil || c.wa == nil || strings.HasPrefix(c.dbPath, "test:") {
		return
	}
	groups, err := c.wa.GetJoinedGroups(ctx)
	if err != nil {
		c.log("fetch WhatsApp joined groups failed: %v", err)
		return
	}
	for _, group := range groups {
		if group == nil || group.JID.IsEmpty() {
			continue
		}
		name := group.Name
		if name == "" {
			name = group.JID.User
		}
		muted := chatMuted(c, ctx, group.JID)
		emitProfileEvent(c, ctx, group.JID, name, "", true, &muted)
	}
	c.log("emitted WhatsApp joined groups count=%d", len(groups))
}

func (c *client) fetchAndEmitProfile(ctx context.Context, jid types.JID, name string, isGroup bool) {
	if c == nil || c.wa == nil || jid.IsEmpty() || strings.HasPrefix(c.dbPath, "test:") {
		return
	}
	if name == "" {
		name = conversationName(c, ctx, jid, "")
	}

	info, err := c.wa.GetProfilePictureInfo(ctx, jid, &whatsmeow.GetProfilePictureParams{Preview: false})
	if err != nil || info == nil || info.URL == "" {
		muted := chatMuted(c, ctx, jid)
		emitProfileEvent(c, ctx, jid, name, "", isGroup, &muted)
		return
	}

	path, err := c.downloadProfilePicture(ctx, jid.String(), info.ID, info.URL)
	if err != nil {
		muted := chatMuted(c, ctx, jid)
		emitProfileEvent(c, ctx, jid, name, "", isGroup, &muted)
		return
	}
	muted := chatMuted(c, ctx, jid)
	emitProfileEvent(c, ctx, jid, name, path, isGroup, &muted)
}

func (c *client) downloadProfilePicture(ctx context.Context, jid, pictureID, url string) (string, error) {
	dir := profileCacheDir(c.dbPath)
	if err := os.MkdirAll(dir, 0o700); err != nil {
		return "", err
	}
	key := "full:" + jid + ":" + pictureID
	if pictureID == "" {
		key = "full:" + jid + ":" + url
	}
	digest := sha256.Sum256([]byte(key))
	path := filepath.Join(dir, hex.EncodeToString(digest[:])+".jpg")
	if _, err := os.Stat(path); err == nil {
		return path, nil
	}

	req, err := http.NewRequestWithContext(ctx, http.MethodGet, url, nil)
	if err != nil {
		return "", err
	}
	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		return "", err
	}
	defer resp.Body.Close()
	if resp.StatusCode < 200 || resp.StatusCode >= 300 {
		return "", fmt.Errorf("profile picture download returned HTTP %d", resp.StatusCode)
	}

	file, err := os.Create(path)
	if err != nil {
		return "", err
	}
	defer file.Close()
	if _, err := io.Copy(file, resp.Body); err != nil {
		return "", err
	}
	return path, nil
}

func pollCreation(message *waProto.Message) *waProto.PollCreationMessage {
	message = displayableMessage(message)
	if message == nil {
		return nil
	}
	for _, poll := range []*waProto.PollCreationMessage{
		message.GetPollCreationMessage(),
		message.GetPollCreationMessageV2(),
		message.GetPollCreationMessageV3(),
		message.GetPollCreationMessageV5(),
	} {
		if poll != nil {
			return poll
		}
	}
	return nil
}

func outboundMediaType(contentType string) whatsmeow.MediaType {
	switch contentType {
	case "image", "sticker":
		return whatsmeow.MediaImage
	case "gif", "video":
		return whatsmeow.MediaVideo
	case "audio":
		return whatsmeow.MediaAudio
	default:
		return whatsmeow.MediaDocument
	}
}

func prepareOutboundMediaUpload(c *client, pathRaw, mimeRaw, contentRaw string) (string, string, string, func(), error) {
	if contentRaw != "gif" {
		return pathRaw, mimeRaw, contentRaw, nil, nil
	}
	converted, err := os.CreateTemp("", "chat-cli-whatsapp-gif-*.mp4")
	if err != nil {
		return "", "", "", nil, fmt.Errorf("prepare WhatsApp GIF conversion: %v", err)
	}
	convertedPath := converted.Name()
	_ = converted.Close()
	cleanup := func() { _ = os.Remove(convertedPath) }
	conversionAttempts := []struct {
		name string
		args []string
	}{
		{
			name: "libx264",
			args: []string{
				"-hide_banner",
				"-loglevel", "error",
				"-y",
				"-i", pathRaw,
				"-an",
				"-c:v", "libx264",
				"-preset", "veryfast",
				"-movflags", "+faststart",
				"-pix_fmt", "yuv420p",
				"-vf", "scale=trunc(iw/2)*2:trunc(ih/2)*2",
				convertedPath,
			},
		},
		{
			name: "h264_v4l2m2m",
			args: []string{
				"-hide_banner",
				"-loglevel", "error",
				"-y",
				"-i", pathRaw,
				"-an",
				"-c:v", "h264_v4l2m2m",
				"-movflags", "+faststart",
				"-pix_fmt", "yuv420p",
				"-vf", "scale=trunc(iw/2)*2:trunc(ih/2)*2",
				convertedPath,
			},
		},
		{
			name: "h264_vulkan",
			args: []string{
				"-hide_banner",
				"-loglevel", "error",
				"-y",
				"-i", pathRaw,
				"-an",
				"-c:v", "h264_vulkan",
				"-movflags", "+faststart",
				"-pix_fmt", "yuv420p",
				"-vf", "scale=trunc(iw/2)*2:trunc(ih/2)*2",
				convertedPath,
			},
		},
	}
	if _, err := os.Stat("/dev/dri/renderD128"); err == nil {
		conversionAttempts = append(conversionAttempts, struct {
			name string
			args []string
		}{
			name: "h264_vaapi",
			args: []string{
				"-hide_banner",
				"-loglevel", "error",
				"-y",
				"-vaapi_device", "/dev/dri/renderD128",
				"-i", pathRaw,
				"-an",
				"-vf", "scale=trunc(iw/2)*2:trunc(ih/2)*2,format=nv12,hwupload",
				"-c:v", "h264_vaapi",
				"-movflags", "+faststart",
				convertedPath,
			},
		})
	}
	var messages []string
	for _, attempt := range conversionAttempts {
		cmd := exec.Command("ffmpeg", attempt.args...)
		output, err := cmd.CombinedOutput()
		if err == nil {
			if c != nil {
				c.log("converted WhatsApp GIF path=%s upload_path=%s encoder=%s", pathRaw, convertedPath, attempt.name)
			}
			return convertedPath, "video/mp4", "gif", cleanup, nil
		}
		message := strings.TrimSpace(string(output))
		if message == "" {
			message = err.Error()
		}
		messages = append(messages, fmt.Sprintf("%s: %s", attempt.name, message))
		if c != nil {
			c.log("convert WhatsApp GIF failed path=%s encoder=%s: %s", pathRaw, attempt.name, message)
		}
	}
	cleanup()
	return "", "", "", nil, fmt.Errorf("convert WhatsApp GIF to playable video: no usable H.264 encoder found. Install an FFmpeg build with libx264 or working H.264 hardware encoding. Attempts: %s", strings.Join(messages, "; "))
}

func normalizeOutboundContentType(contentType, mimeType string) string {
	mimeType = strings.ToLower(strings.TrimSpace(mimeType))
	if mimeType == "image/gif" {
		return "gif"
	}
	switch contentType {
	case "image", "video", "audio", "file", "sticker":
		return contentType
	case "gif":
		return "gif"
	}
	if strings.HasPrefix(mimeType, "image/") {
		if mimeType == "image/webp" {
			return "sticker"
		}
		return "image"
	}
	if strings.HasPrefix(mimeType, "video/") {
		return "video"
	}
	if strings.HasPrefix(mimeType, "audio/") {
		return "audio"
	}
	if mimeType != "" {
		return "file"
	}
	return ""
}

func outboundMediaMessage(contentType string, upload whatsmeow.UploadResponse, mimeType, fileName, caption string) *waProto.Message {
	mediaKeyTimestamp := proto.Int64(time.Now().Unix())
	switch contentType {
	case "image":
		return &waProto.Message{ImageMessage: &waProto.ImageMessage{
			URL:           proto.String(upload.URL),
			Mimetype:      proto.String(mimeType),
			Caption:       proto.String(caption),
			FileSHA256:    upload.FileSHA256,
			FileLength:    proto.Uint64(upload.FileLength),
			MediaKey:      upload.MediaKey,
			FileEncSHA256: upload.FileEncSHA256,
			DirectPath:    proto.String(upload.DirectPath),
		}}
	case "gif":
		return &waProto.Message{VideoMessage: &waProto.VideoMessage{
			URL:               proto.String(upload.URL),
			Mimetype:          proto.String(mimeType),
			FileSHA256:        upload.FileSHA256,
			FileLength:        proto.Uint64(upload.FileLength),
			Seconds:           proto.Uint32(1),
			MediaKey:          upload.MediaKey,
			Caption:           proto.String(caption),
			FileEncSHA256:     upload.FileEncSHA256,
			DirectPath:        proto.String(upload.DirectPath),
			MediaKeyTimestamp: mediaKeyTimestamp,
			GifPlayback:       proto.Bool(true),
		}}
	case "video":
		return &waProto.Message{VideoMessage: &waProto.VideoMessage{
			URL:               proto.String(upload.URL),
			Mimetype:          proto.String(mimeType),
			FileSHA256:        upload.FileSHA256,
			FileLength:        proto.Uint64(upload.FileLength),
			MediaKey:          upload.MediaKey,
			Caption:           proto.String(caption),
			FileEncSHA256:     upload.FileEncSHA256,
			DirectPath:        proto.String(upload.DirectPath),
			MediaKeyTimestamp: mediaKeyTimestamp,
			GifPlayback:       proto.Bool(false),
		}}
	case "audio":
		return &waProto.Message{AudioMessage: &waProto.AudioMessage{
			URL:           proto.String(upload.URL),
			Mimetype:      proto.String(mimeType),
			FileSHA256:    upload.FileSHA256,
			FileLength:    proto.Uint64(upload.FileLength),
			MediaKey:      upload.MediaKey,
			FileEncSHA256: upload.FileEncSHA256,
			DirectPath:    proto.String(upload.DirectPath),
		}}
	case "sticker":
		return &waProto.Message{StickerMessage: &waProto.StickerMessage{
			URL:               proto.String(upload.URL),
			FileSHA256:        upload.FileSHA256,
			FileEncSHA256:     upload.FileEncSHA256,
			MediaKey:          upload.MediaKey,
			Mimetype:          proto.String(firstNonEmpty(mimeType, "image/webp")),
			DirectPath:        proto.String(upload.DirectPath),
			FileLength:        proto.Uint64(upload.FileLength),
			MediaKeyTimestamp: mediaKeyTimestamp,
		}}
	default:
		return &waProto.Message{DocumentMessage: &waProto.DocumentMessage{
			URL:           proto.String(upload.URL),
			Mimetype:      proto.String(mimeType),
			Title:         proto.String(fileName),
			FileSHA256:    upload.FileSHA256,
			FileLength:    proto.Uint64(upload.FileLength),
			MediaKey:      upload.MediaKey,
			FileName:      proto.String(fileName),
			FileEncSHA256: upload.FileEncSHA256,
			DirectPath:    proto.String(upload.DirectPath),
			Caption:       proto.String(caption),
		}}
	}
}

func applyMedia(c *client, messageID string, message *waProto.Message, event *bridgeEvent) {
	message = displayableMessage(message)
	if message == nil || event == nil {
		return
	}
	ctx := context.Background()
	if image := message.GetImageMessage(); image != nil {
		event.ContentType = "image"
		event.MediaID = mediaID(messageID, image.GetDirectPath())
		event.MediaMime = firstNonEmpty(image.GetMimetype(), "image/jpeg")
		event.MediaSize = image.GetFileLength()
		event.Caption = image.GetCaption()
		event.Text = image.GetCaption()
		event.MediaFileName = mediaFileName(event.MediaID, event.MediaMime, "jpg")
		event.MediaThumbnail = c.writeMediaThumbnail(event.MediaID, image.GetJPEGThumbnail(), "jpg")
		event.MediaLocalPath = c.downloadMedia(ctx, event.MediaID, event.MediaFileName, image)
		return
	}
	if video := message.GetVideoMessage(); video != nil {
		event.ContentType = "video"
		event.MediaID = mediaID(messageID, video.GetDirectPath())
		event.MediaMime = firstNonEmpty(video.GetMimetype(), "video/mp4")
		event.MediaSize = video.GetFileLength()
		event.Caption = video.GetCaption()
		event.Text = video.GetCaption()
		event.MediaFileName = mediaFileName(event.MediaID, event.MediaMime, "mp4")
		event.MediaThumbnail = c.writeMediaThumbnail(event.MediaID, video.GetJPEGThumbnail(), "jpg")
		event.MediaLocalPath = c.downloadMedia(ctx, event.MediaID, event.MediaFileName, video)
		return
	}
	if audio := message.GetAudioMessage(); audio != nil {
		event.ContentType = "audio"
		event.MediaID = mediaID(messageID, audio.GetDirectPath())
		event.MediaMime = firstNonEmpty(audio.GetMimetype(), "audio/ogg")
		event.MediaSize = audio.GetFileLength()
		event.MediaFileName = mediaFileName(event.MediaID, event.MediaMime, "ogg")
		event.MediaLocalPath = c.downloadMedia(ctx, event.MediaID, event.MediaFileName, audio)
		return
	}
	if document := message.GetDocumentMessage(); document != nil {
		event.ContentType = "file"
		event.MediaID = mediaID(messageID, document.GetDirectPath())
		event.MediaMime = firstNonEmpty(document.GetMimetype(), "application/octet-stream")
		event.MediaSize = document.GetFileLength()
		event.Caption = document.GetCaption()
		event.Text = document.GetCaption()
		event.MediaFileName = firstNonEmpty(document.GetFileName(), mediaFileName(event.MediaID, event.MediaMime, "bin"))
		event.MediaLocalPath = c.downloadMedia(ctx, event.MediaID, event.MediaFileName, document)
		return
	}
	if sticker := message.GetStickerMessage(); sticker != nil {
		event.ContentType = "sticker"
		event.MediaID = mediaID(messageID, sticker.GetDirectPath())
		event.MediaMime = firstNonEmpty(sticker.GetMimetype(), "image/webp")
		event.MediaSize = sticker.GetFileLength()
		event.MediaFileName = mediaFileName(event.MediaID, event.MediaMime, "webp")
		event.MediaLocalPath = c.downloadMedia(ctx, event.MediaID, event.MediaFileName, sticker)
		return
	}
}

func (c *client) downloadMedia(ctx context.Context, mediaID, fileName string, media whatsmeow.DownloadableMessage) string {
	if c == nil || c.wa == nil || media == nil || strings.HasPrefix(c.dbPath, "test:") {
		return ""
	}
	dir := mediaCacheDir(c.dbPath)
	if err := os.MkdirAll(dir, 0o700); err != nil {
		c.log("create media cache failed: %v", err)
		return ""
	}
	path := filepath.Join(dir, safeFileName(fileName))
	if info, err := os.Stat(path); err == nil && info.Size() > 0 {
		c.log("reused cached WhatsApp media %s", path)
		return path
	}

	data, err := c.wa.Download(ctx, media)
	if err != nil || len(data) == 0 {
		c.log("download WhatsApp media failed media_id=%s: %v", mediaID, err)
		return ""
	}
	if err := os.WriteFile(path, data, 0o600); err != nil {
		c.log("write WhatsApp media failed path=%s: %v", path, err)
		return ""
	}
	c.log("downloaded WhatsApp media %s bytes=%d", path, len(data))
	return path
}

func (c *client) writeMediaThumbnail(mediaID string, data []byte, fallbackExt string) string {
	if c == nil || len(data) == 0 || strings.HasPrefix(c.dbPath, "test:") {
		return ""
	}
	dir := mediaCacheDir(c.dbPath)
	if err := os.MkdirAll(dir, 0o700); err != nil {
		return ""
	}
	path := filepath.Join(dir, safeFileName(mediaID+"-thumb."+fallbackExt))
	if err := os.WriteFile(path, data, 0o600); err != nil {
		return ""
	}
	return path
}

func shouldSkipUnsupportedDisplayMessage(event bridgeEvent) bool {
	return event.Text == "[unsupported WhatsApp message]" &&
		event.ContentType == "" &&
		event.MediaID == "" &&
		event.ReactionMessageID == "" &&
		len(event.Reactions) == 0
}

func mediaID(messageID, directPath string) string {
	key := messageID
	if directPath != "" {
		key += ":" + directPath
	}
	digest := sha256.Sum256([]byte(key))
	return hex.EncodeToString(digest[:])
}

func mediaFileName(mediaID, mime, fallbackExt string) string {
	ext := extensionForMime(mime, fallbackExt)
	return mediaID + "." + ext
}

func extensionForMime(mime, fallback string) string {
	switch strings.ToLower(strings.TrimSpace(mime)) {
	case "image/jpeg", "image/jpg":
		return "jpg"
	case "image/png":
		return "png"
	case "image/webp":
		return "webp"
	case "video/mp4":
		return "mp4"
	case "audio/ogg", "audio/opus":
		return "ogg"
	case "audio/mpeg":
		return "mp3"
	case "application/pdf":
		return "pdf"
	default:
		return fallback
	}
}

func safeFileName(name string) string {
	name = filepath.Base(name)
	name = strings.ReplaceAll(name, string(os.PathSeparator), "_")
	if name == "." || name == "" {
		return "media.bin"
	}
	return name
}

func firstNonEmpty(values ...string) string {
	for _, value := range values {
		if value != "" {
			return value
		}
	}
	return ""
}

func mediaCacheDir(dbPath string) string {
	return filepath.Join(whatsappCacheRoot(), "media")
}

func profileCacheDir(dbPath string) string {
	return filepath.Join(whatsappCacheRoot(), "avatars")
}

func whatsappCacheRoot() string {
	if cacheHome := strings.TrimSpace(os.Getenv("XDG_CACHE_HOME")); cacheHome != "" {
		return filepath.Join(cacheHome, "chat-cli", "whatsapp")
	}
	if home := strings.TrimSpace(os.Getenv("HOME")); home != "" {
		return filepath.Join(home, ".cache", "chat-cli", "whatsapp")
	}
	return filepath.Join(os.TempDir(), "chat-cli", "whatsapp")
}

// messageMentionedJID returns the list of JIDs mentioned in the message via the
// protocol ContextInfo, across the common message variants that carry it.
func messageMentionedJID(message *waProto.Message) []string {
	message = displayableMessage(message)
	if message == nil {
		return nil
	}
	var info *waProto.ContextInfo
	switch {
	case message.GetExtendedTextMessage() != nil:
		info = message.GetExtendedTextMessage().GetContextInfo()
	case message.GetImageMessage() != nil:
		info = message.GetImageMessage().GetContextInfo()
	case message.GetVideoMessage() != nil:
		info = message.GetVideoMessage().GetContextInfo()
	case message.GetAudioMessage() != nil:
		info = message.GetAudioMessage().GetContextInfo()
	case message.GetDocumentMessage() != nil:
		info = message.GetDocumentMessage().GetContextInfo()
	case message.GetStickerMessage() != nil:
		info = message.GetStickerMessage().GetContextInfo()
	}
	if info == nil {
		return nil
	}
	return info.GetMentionedJID()
}

// messageMentionsUser reports whether the authenticated user (ownJID) is among
// the JIDs mentioned in the message. Comparison is on the JID user-part so that
// device/agent suffixes do not affect the match.
func messageMentionsUser(message *waProto.Message, ownJID string) bool {
	if ownJID == "" {
		return false
	}
	own, err := types.ParseJID(ownJID)
	if err != nil {
		return false
	}
	for _, mentioned := range messageMentionedJID(message) {
		parsed, err := types.ParseJID(mentioned)
		if err != nil {
			continue
		}
		if parsed.User != "" && parsed.User == own.User {
			return true
		}
	}
	return false
}

// resolveMentions rewrites WhatsApp @<jid-user> mention tokens in a message
// body to the mentioned contact's display name, e.g. "@34819417346247" becomes
// "@Razvan". WhatsApp transmits mentions as the bare JID user-part inside the
// text plus a parallel MentionedJID list in the ContextInfo; without this
// rewrite the UI shows the raw phone number / LID instead of the person's name.
// JIDs that cannot be resolved to a real name are left as-is so the mention is
// never dropped.
func resolveMentions(c *client, ctx context.Context, text string, mentionedJIDs []string) string {
	if text == "" || len(mentionedJIDs) == 0 {
		return text
	}
	names := make(map[string]string, len(mentionedJIDs))
	for _, raw := range mentionedJIDs {
		jid, err := types.ParseJID(raw)
		if err != nil || jid.User == "" {
			continue
		}
		if _, ok := names[jid.User]; ok {
			continue
		}
		if name, ok := contactDisplayName(c, ctx, jid); ok {
			if trimmed := strings.TrimSpace(name); trimmed != "" {
				names[jid.User] = trimmed
			}
		}
	}
	return rewriteMentionTokens(text, names)
}

// rewriteMentionTokens replaces each "@<user>" token in text with "@<name>"
// using the user-part -> display-name map. Tokens are applied longest user-part
// first so a shorter number that is a prefix of a longer one (for example
// "@1234" versus "@12345") cannot be partially rewritten.
func rewriteMentionTokens(text string, names map[string]string) string {
	if text == "" || len(names) == 0 {
		return text
	}
	users := make([]string, 0, len(names))
	for user := range names {
		users = append(users, user)
	}
	sort.Slice(users, func(i, j int) bool {
		return len(users[i]) > len(users[j])
	})
	for _, user := range users {
		text = strings.ReplaceAll(text, "@"+user, "@"+names[user])
	}
	return text
}

// conversationActivity derives last-message activity for a history-synced
// conversation from the actual messages included in the payload (newest
// displayable message wins). When the payload carries no usable message —
// common for INITIAL_BOOTSTRAP conversations whose messages fall outside the
// sync scope window — it falls back to the conversation's own last-message
// timestamp, which still describes a real message even though its body is
// unavailable. Returns a zero time when no activity is known.
func conversationActivity(c *client, ctx context.Context, messages []*events.Message, lastMsgTimestamp uint64) (time.Time, string) {
	var newest time.Time
	preview := ""
	for _, message := range messages {
		if message == nil || displayableMessage(message.Message) == nil {
			continue
		}
		payload := displayableMessage(message.Message)
		if payload.GetReactionMessage() != nil ||
			payload.GetEncReactionMessage() != nil ||
			payload.GetPollUpdateMessage() != nil ||
			payload.GetProtocolMessage() != nil {
			continue
		}
		// A history-synced edit carries the edit time, not message activity.
		if _, isEdit := detectMessageEdit(message); isEdit {
			continue
		}
		if !message.Info.Timestamp.After(newest) {
			continue
		}
		text := resolveMentions(c, ctx, messageText(payload), messageMentionedJID(payload))
		if text == "[unsupported WhatsApp message]" {
			continue
		}
		if text == "" && payload.GetImageMessage() != nil {
			text = "[image]"
		}
		newest = message.Info.Timestamp
		preview = text
	}
	if newest.IsZero() && lastMsgTimestamp > 0 {
		newest = time.Unix(int64(lastMsgTimestamp), 0).UTC()
	}
	return newest, preview
}

func messageText(message *waProto.Message) string {
	message = displayableMessage(message)
	if message == nil {
		return ""
	}
	if text := message.GetConversation(); text != "" {
		return text
	}
	if extended := message.GetExtendedTextMessage(); extended != nil {
		return extended.GetText()
	}
	if image := message.GetImageMessage(); image != nil {
		return image.GetCaption()
	}
	if video := message.GetVideoMessage(); video != nil {
		return imageCaption(video.GetCaption(), "[video]")
	}
	if audio := message.GetAudioMessage(); audio != nil {
		return "[audio]"
	}
	if document := message.GetDocumentMessage(); document != nil {
		return imageCaption(document.GetCaption(), "[file]")
	}
	if sticker := message.GetStickerMessage(); sticker != nil {
		return "[sticker]"
	}
	if poll := pollCreation(message); poll != nil {
		return poll.GetName()
	}
	return "[unsupported WhatsApp message]"
}

func imageCaption(caption, fallback string) string {
	if caption != "" {
		return caption
	}
	return fallback
}

func (c *client) shouldSkipHistoryMessage(timestamp time.Time) bool {
	switch c.syncScope {
	case "today":
		now := time.Now().Local()
		start := time.Date(now.Year(), now.Month(), now.Day(), 0, 0, 0, 0, now.Location())
		return timestamp.Before(start)
	case "none":
		return true
	default:
		return false
	}
}

func (c *client) log(format string, args ...interface{}) {
	if c == nil || strings.TrimSpace(c.logPath) == "" {
		return
	}
	line := fmt.Sprintf(format, args...)
	entry := fmt.Sprintf("%s whatsapp-bridge %s\n", time.Now().UTC().Format(time.RFC3339Nano), line)
	_ = os.MkdirAll(filepath.Dir(c.logPath), 0o700)
	file, err := os.OpenFile(c.logPath, os.O_CREATE|os.O_WRONLY|os.O_APPEND, 0o600)
	if err != nil {
		return
	}
	defer file.Close()
	_, _ = file.WriteString(entry)
}

func sqliteDSN(path string) string {
	if path == "" || path == ":memory:" {
		return ":memory:?_foreign_keys=on"
	}
	if strings.HasPrefix(path, "file:") {
		if strings.Contains(path, "?") {
			return path + "&_foreign_keys=on"
		}
		return path + "?_foreign_keys=on"
	}
	return "file:" + path + "?_foreign_keys=on"
}

func emit(event bridgeEvent) {
	payload, err := json.Marshal(event)
	if err != nil {
		payload, _ = json.Marshal(bridgeEvent{Type: "error", Message: err.Error()})
	}
	emitRaw(string(payload))
}

func emitRaw(payload string) {
	mu.Lock()
	cb := msgCb
	ctx := msgCbCtx
	mu.Unlock()

	if cb == nil {
		return
	}

	copy := C.CString(payload)
	defer C.free(unsafe.Pointer(copy))
	C.call_message_callback(cb, copy, ctx)
}

func cJSON(event bridgeEvent) *C.char {
	payload, err := json.Marshal(event)
	if err != nil {
		payload, _ = json.Marshal(bridgeEvent{Type: "error", Message: err.Error()})
	}
	return C.CString(string(payload))
}
