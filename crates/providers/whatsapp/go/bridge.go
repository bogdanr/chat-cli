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
	"time"
	"unsafe"

	_ "github.com/mattn/go-sqlite3"
	"go.mau.fi/whatsmeow"
	"go.mau.fi/whatsmeow/appstate"
	waProto "go.mau.fi/whatsmeow/binary/proto"
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
	IsGroup      bool   `json:"is_group,omitempty"`
	Muted        *bool  `json:"muted,omitempty"`
	Progress     uint8  `json:"progress,omitempty"`

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
	container, err := sqlstore.New(ctx, "sqlite3", sqliteDSN(c.dbPath), waLog.Noop)
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

	wa := whatsmeow.NewClient(device, waLog.Noop)
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
				if evt.Event == "code" {
					emit(bridgeEvent{Type: "qr", Code: evt.Code})
				} else {
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
		go c.syncChatMuteSettings(context.Background())
		go c.emitJoinedGroups(context.Background())
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

//export C_SendText
func C_SendText(clientID C.uint64_t, chatJID *C.char, text *C.char) *C.char {
	mu.Lock()
	c, ok := clients[uint64(clientID)]
	mu.Unlock()
	if !ok {
		return cJSON(bridgeEvent{Type: "error", Message: "WhatsApp bridge client is not connected"})
	}

	body := C.GoString(text)
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

	resp, err := c.wa.SendMessage(context.Background(), jid, &waProto.Message{
		Conversation: proto.String(body),
	})
	if err != nil {
		return cJSON(bridgeEvent{Type: "error", Message: fmt.Sprintf("send WhatsApp message: %v", err)})
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
func C_SendMedia(clientID C.uint64_t, chatJID *C.char, path *C.char, mimeType *C.char, fileName *C.char, caption *C.char, contentType *C.char) *C.char {
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
	resp, err := c.wa.SendMessage(context.Background(), jid, message)
	if err != nil {
		c.log("send WhatsApp media failed chat=%s path=%s mime=%s content_type=%s: %v", chatRaw, dataPath, messageMime, messageContentType, err)
		return cJSON(bridgeEvent{Type: "error", Message: fmt.Sprintf("send WhatsApp media: %v", err)})
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
		return cJSON(bridgeEvent{Type: "error", Message: fmt.Sprintf("send WhatsApp reaction: %v", err)})
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
		return cJSON(bridgeEvent{Type: "error", Message: fmt.Sprintf("send WhatsApp poll vote: %v", err)})
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
	alt, err := c.wa.Store.GetAltJID(ctx, jid)
	if err != nil || alt.IsEmpty() {
		return jidAlias{canonical: jid}
	}
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
	alias := canonicalJIDAlias(c, ctx, jid)
	event := bridgeEvent{Type: "profile", JID: alias.canonical.String(), SenderName: name, AvatarPath: avatarPath, IsGroup: isGroup, Muted: muted}
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

func handleWhatsAppEvent(c *client, evt interface{}) {
	switch v := evt.(type) {
	case *events.Message:
		emitMessageEvent(c, v, "message")
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
	case *events.Connected:
		emit(bridgeEvent{Type: "connected"})
	case *events.LoggedOut:
		emit(bridgeEvent{Type: "disconnected", Reason: "logged out"})
	case *events.Disconnected:
		emit(bridgeEvent{Type: "disconnected"})
	case *events.StreamReplaced:
		emit(bridgeEvent{Type: "disconnected", Reason: "stream replaced"})
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
		emitProfileEvent(c, ctx, chatJID, chatName, "", isGroup, &chatMuted)
		go c.fetchAndEmitProfile(ctx, chatJID, chatName, isGroup)

		for _, historyMsg := range conv.GetMessages() {
			webMessage := historyMsg.GetMessage()
			if webMessage == nil {
				continue
			}
			message, err := c.wa.ParseWebMessage(chatJID, webMessage)
			if err != nil {
				continue
			}
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

func emitMessageEvent(c *client, message *events.Message, eventType string) {
	if message == nil {
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

	if reaction := message.Message.GetReactionMessage(); reaction != nil {
		emitReactionMessageEvent(c, message, reaction, chatJID, chatName, senderName, isGroup)
		return
	}
	if message.Message.GetEncReactionMessage() != nil {
		if c != nil && c.wa != nil {
			reaction, err := c.wa.DecryptReaction(ctx, message)
			if err == nil && reaction != nil {
				emitReactionMessageEvent(c, message, reaction, chatJID, chatName, senderName, isGroup)
			}
		}
		return
	}
	if message.Message.GetPollUpdateMessage() != nil {
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
		Text:       messageText(message.Message),
		Timestamp:  message.Info.Timestamp.UTC().Format(time.RFC3339Nano),
		FromMe:     message.Info.IsFromMe,
		IsGroup:    isGroup,
		Muted:      boolPtr(chatMuted(c, ctx, chatJID)),
		Reactions:  messageReactions(message),
	}
	if !canonicalChatAlias.alternate.IsEmpty() && canonicalChatAlias.alternate != canonicalChatAlias.canonical {
		event.CanonicalJID = canonicalChatAlias.canonical.String()
		event.AltJID = canonicalChatAlias.alternate.String()
	}
	if poll := pollCreation(message.Message); poll != nil {
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
		applyMedia(c, message.Info.ID, message.Message, &event)
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

func messageReactions(message *events.Message) []bridgeReaction {
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
	if alt, err := c.wa.Store.GetAltJID(ctx, jid); err == nil && !alt.IsEmpty() {
		return contactDisplayNameForJID(c, ctx, alt)
	}
	return "", false
}

func contactDisplayNameForJID(c *client, ctx context.Context, jid types.JID) (string, bool) {
	if contact, err := c.wa.Store.Contacts.GetContact(ctx, jid); err == nil {
		if name := displayNameForContact(contact, jid); name != "" {
			return name, true
		}
	}
	return "", false
}

func displayNameForContact(contact types.ContactInfo, jid types.JID) string {
	for _, name := range []string{contact.FullName, contact.FirstName, contact.BusinessName, contact.PushName} {
		if strings.TrimSpace(name) != "" {
			return name
		}
	}
	return jid.User
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
	if dbPath == "" || dbPath == ":memory:" || strings.HasPrefix(dbPath, "file:") {
		return filepath.Join(os.TempDir(), "chat-cli-whatsapp-media")
	}
	return dbPath + ".media"
}

func profileCacheDir(dbPath string) string {
	if dbPath == "" || dbPath == ":memory:" || strings.HasPrefix(dbPath, "file:") {
		return filepath.Join(os.TempDir(), "chat-cli-whatsapp-avatars")
	}
	return dbPath + ".avatars"
}

func messageText(message *waProto.Message) string {
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
