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
	"net/http"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"time"
	"unsafe"

	_ "github.com/mattn/go-sqlite3"
	"go.mau.fi/whatsmeow"
	waProto "go.mau.fi/whatsmeow/binary/proto"
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

type bridgeEvent struct {
	Type       string `json:"type"`
	Code       string `json:"code,omitempty"`
	Event      string `json:"event,omitempty"`
	Message    string `json:"message,omitempty"`
	Reason     string `json:"reason,omitempty"`
	JID        string `json:"jid,omitempty"`
	ID         string `json:"id,omitempty"`
	ChatJID    string `json:"chat_jid,omitempty"`
	ChatName   string `json:"chat_name,omitempty"`
	SenderJID  string `json:"sender_jid,omitempty"`
	SenderName string `json:"sender_name,omitempty"`
	AvatarPath string `json:"avatar_path,omitempty"`
	Text       string `json:"text,omitempty"`
	Timestamp  string `json:"timestamp,omitempty"`
	FromMe     bool   `json:"from_me,omitempty"`
	IsGroup    bool   `json:"is_group,omitempty"`
	Progress   uint8  `json:"progress,omitempty"`

	ContentType       string `json:"content_type,omitempty"`
	MediaID           string `json:"media_id,omitempty"`
	MediaFileName     string `json:"media_file_name,omitempty"`
	MediaMime         string `json:"media_mime,omitempty"`
	MediaSize         uint64 `json:"media_size,omitempty"`
	MediaLocalPath    string `json:"media_local_path,omitempty"`
	MediaThumbnail    string `json:"media_thumbnail_path,omitempty"`
	Caption           string   `json:"caption,omitempty"`
	ReactionMessageID string   `json:"reaction_message_id,omitempty"`
	ReactionEmoji     string   `json:"reaction_emoji,omitempty"`
	PollQuestion      string   `json:"poll_question,omitempty"`
	PollOptions       []string `json:"poll_options,omitempty"`
	PollSelectable    uint32   `json:"poll_selectable_options_count,omitempty"`
}

var (
	mu       sync.Mutex
	clients  = map[uint64]*client{}
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
	return cJSON(bridgeEvent{
		Type:      "sent",
		ID:        id,
		ChatJID:   jid.String(),
		SenderJID: c.ownJID(),
		Text:      body,
		Timestamp: time.Now().UTC().Format(time.RFC3339Nano),
		FromMe:    true,
		IsGroup:   strings.HasSuffix(jid.Server, "g.us"),
	})
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

func handleWhatsAppEvent(c *client, evt interface{}) {
	switch v := evt.(type) {
	case *events.Message:
		emitMessageEvent(c, v, "message")
	case *events.HistorySync:
		emitHistorySync(c, v)
	case *events.PushName:
		jid := v.JID.String()
		emit(bridgeEvent{Type: "profile", JID: jid, SenderName: v.NewPushName})
	case *events.Picture:
		if v.Remove {
			emit(bridgeEvent{Type: "profile", JID: v.JID.String()})
		} else {
			go c.fetchAndEmitProfile(context.Background(), v.JID, "", v.JID.Server == types.GroupServer)
		}
	case *events.GroupInfo:
		if v.Name != nil && v.Name.Name != "" {
			emit(bridgeEvent{Type: "profile", JID: v.JID.String(), SenderName: v.Name.Name, IsGroup: true})
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
	if c.syncScope == "none" {
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
			emit(bridgeEvent{Type: "profile", JID: jid.String(), SenderName: pushName.GetPushname()})
		}
	}

	for _, conv := range evt.Data.GetConversations() {
		chatJID, err := types.ParseJID(conv.GetID())
		if err != nil || chatJID.IsEmpty() {
			continue
		}
		isGroup := chatJID.Server == types.GroupServer
		chatName := conversationName(c, ctx, chatJID, firstNonEmpty(conv.GetDisplayName(), conv.GetName()))
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
			if c.shouldSkipHistoryMessage(message.Info.Timestamp) {
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
	chatJID := message.Info.Chat
	if message.Info.IsIncomingBroadcast() {
		chatJID = message.Info.Sender
	}
	isGroup := message.Info.IsGroup
	chatName := ""
	if c != nil {
		chatName = conversationName(c, context.Background(), chatJID, "")
	}
	if chatName == "" {
		chatName = message.Info.PushName
	}

	if chatName == "" && isGroup && c != nil && c.wa != nil {
		go c.fetchAndEmitGroupInfo(context.Background(), chatJID)
	}

	if reaction := message.Message.GetReactionMessage(); reaction != nil {
		targetID := reaction.GetKey().GetID()
		if targetID != "" {
			emit(bridgeEvent{
				Type:              "reaction",
				ID:                message.Info.ID,
				ChatJID:           chatJID.String(),
				ChatName:          chatName,
				SenderJID:         message.Info.Sender.String(),
				SenderName:        message.Info.PushName,
				Timestamp:         message.Info.Timestamp.UTC().Format(time.RFC3339Nano),
				FromMe:            message.Info.IsFromMe,
				IsGroup:           isGroup,
				ReactionMessageID: targetID,
				ReactionEmoji:     reaction.GetText(),
			})
		}
		return
	}

	event := bridgeEvent{
		Type:       eventType,
		ID:         message.Info.ID,
		ChatJID:    chatJID.String(),
		ChatName:   chatName,
		SenderJID:  message.Info.Sender.String(),
		SenderName: message.Info.PushName,
		Text:       messageText(message.Message),
		Timestamp:  message.Info.Timestamp.UTC().Format(time.RFC3339Nano),
		FromMe:     message.Info.IsFromMe,
		IsGroup:    isGroup,
	}
	if poll := pollCreation(message.Message); poll != nil {
		event.ContentType = "poll"
		event.PollQuestion = poll.GetName()
		event.Text = poll.GetName()
		event.PollSelectable = poll.GetSelectableOptionsCount()
		for _, option := range poll.GetOptions() {
			if name := option.GetOptionName(); name != "" {
				event.PollOptions = append(event.PollOptions, name)
			}
		}
	} else {
		applyMedia(c, message.Info.ID, message.Message, &event)
	}
	emit(event)

	if c != nil {
		ctx := context.Background()
		profileTarget := chatJID
		profileName := chatName
		if !isGroup && !message.Info.Sender.IsEmpty() && !message.Info.IsFromMe {
			profileTarget = message.Info.Sender
			profileName = message.Info.PushName
		}
		go c.fetchAndEmitProfile(ctx, profileTarget, profileName, isGroup)
		if isGroup && !message.Info.Sender.IsEmpty() {
			go c.fetchAndEmitProfile(ctx, message.Info.Sender, message.Info.PushName, false)
		}
	}
}

func conversationName(c *client, ctx context.Context, jid types.JID, fallback string) string {
	if fallback != "" {
		return fallback
	}
	if jid.Server == types.GroupServer && c != nil && c.wa != nil {
		if info, err := c.wa.GetGroupInfo(ctx, jid); err == nil && info != nil && info.Name != "" {
			return info.Name
		}
	}
	if c != nil && c.wa != nil && c.wa.Store != nil && c.wa.Store.Contacts != nil {
		if contact, err := c.wa.Store.Contacts.GetContact(ctx, jid); err == nil {
			for _, name := range []string{contact.FullName, contact.FirstName, contact.BusinessName, contact.PushName} {
				if name != "" {
					return name
				}
			}
		}
	}
	if fallback != "" {
		return fallback
	}
	return jid.User
}

func (c *client) fetchAndEmitGroupInfo(ctx context.Context, jid types.JID) {
	if c == nil || c.wa == nil || jid.IsEmpty() || jid.Server != types.GroupServer || strings.HasPrefix(c.dbPath, "test:") {
		return
	}
	info, err := c.wa.GetGroupInfo(ctx, jid)
	if err == nil && info != nil && info.Name != "" {
		emit(bridgeEvent{Type: "profile", JID: jid.String(), SenderName: info.Name, IsGroup: true})
	}
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
		emit(bridgeEvent{Type: "profile", JID: jid.String(), SenderName: name, IsGroup: isGroup})
		return
	}

	path, err := c.downloadProfilePicture(ctx, jid.String(), info.ID, info.URL)
	if err != nil {
		emit(bridgeEvent{Type: "profile", JID: jid.String(), SenderName: name, IsGroup: isGroup})
		return
	}
	emit(bridgeEvent{Type: "profile", JID: jid.String(), SenderName: name, AvatarPath: path, IsGroup: isGroup})
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
