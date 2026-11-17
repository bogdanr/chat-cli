package main

import (
	"context"
	"fmt"
	"net/http"
	"os"
	"strconv"
	"strings"
	"time"

	"go.mau.fi/whatsmeow"
	"go.mau.fi/whatsmeow/store"
	"go.mau.fi/whatsmeow/types/events"
)

// WhatsApp rejects companions that advertise a web client version it no
// longer accepts (connect failure 405, events.ClientOutdated). whatsmeow pins
// that version at build time and treats the rejection as a permanent,
// expected disconnect, so an older chat-cli binary used to stop syncing
// silently while still looking online.
//
// The bridge recovers without user action: on ClientOutdated it fetches the
// version web.whatsapp.com currently serves, applies it, saves it next to the
// session DB so later launches start with it, and reconnects. A retry only
// happens when the fetched version is strictly newer than the rejected one,
// which bounds the loop. When recovery is impossible the user gets a
// disconnect reason and a system alert instead of silence.

// emitEvent is a package variable so tests can capture recovery events.
var emitEvent = emit

// waVersionFetchTimeout bounds the web.whatsapp.com version lookup.
const waVersionFetchTimeout = 15 * time.Second

// fetchLatestWAVersion is a package variable so tests can stub the network.
var fetchLatestWAVersion = func(ctx context.Context) (store.WAVersionContainer, error) {
	version, err := whatsmeow.GetLatestVersion(ctx, &http.Client{Timeout: waVersionFetchTimeout})
	if err != nil {
		return store.WAVersionContainer{}, err
	}
	if version == nil {
		return store.WAVersionContainer{}, fmt.Errorf("empty version response")
	}
	return *version, nil
}

// reconnectWhatsApp is a package variable so tests can observe reconnects
// without a live websocket.
var reconnectWhatsApp = func(c *client) error {
	if c == nil || c.wa == nil {
		return fmt.Errorf("WhatsApp client not initialized")
	}
	// The rejected socket is normally already closed by now; Disconnect is a
	// no-op in that case and guarantees Connect does not fail with
	// ErrAlreadyConnected if it is not.
	c.wa.Disconnect()
	return c.wa.Connect()
}

// waVersionCachePath is where the last version WhatsApp accepted is kept,
// alongside the session DB like the passkey file. Empty for in-memory and
// test stores.
func waVersionCachePath(dbPath string) string {
	dbPath = strings.TrimSpace(dbPath)
	if dbPath == "" || dbPath == ":memory:" || strings.HasPrefix(dbPath, "test:") {
		return ""
	}
	return dbPath + ".waversion"
}

// parseWAVersion parses a dotted "2.3000.1054933766" version.
func parseWAVersion(raw string) (store.WAVersionContainer, bool) {
	parts := strings.Split(strings.TrimSpace(raw), ".")
	if len(parts) != 3 {
		return store.WAVersionContainer{}, false
	}
	var version store.WAVersionContainer
	for i, part := range parts {
		value, err := strconv.ParseUint(part, 10, 32)
		if err != nil {
			return store.WAVersionContainer{}, false
		}
		version[i] = uint32(value)
	}
	if version.IsZero() {
		return store.WAVersionContainer{}, false
	}
	return version, true
}

// waVersionNewer reports whether a is strictly newer than b.
func waVersionNewer(a, b store.WAVersionContainer) bool {
	for i := range a {
		if a[i] != b[i] {
			return a[i] > b[i]
		}
	}
	return false
}

// applyCachedWAVersion switches to a previously saved version when it is
// newer than the one compiled in. A whatsmeow upgrade that ships a newer
// version wins over an older cached one.
func applyCachedWAVersion(c *client) {
	if c == nil {
		return
	}
	path := waVersionCachePath(c.dbPath)
	if path == "" {
		return
	}
	raw, err := os.ReadFile(path)
	if err != nil {
		if !os.IsNotExist(err) {
			c.log("read cached WhatsApp client version failed: %v", err)
		}
		return
	}
	cached, ok := parseWAVersion(string(raw))
	if !ok {
		c.log("ignoring malformed cached WhatsApp client version %q", strings.TrimSpace(string(raw)))
		return
	}
	current := store.GetWAVersion()
	if waVersionNewer(cached, current) {
		store.SetWAVersion(cached)
		c.log("using cached WhatsApp client version %s (built-in %s)", cached, current)
	}
}

func saveWAVersion(c *client, version store.WAVersionContainer) {
	path := waVersionCachePath(c.dbPath)
	if path == "" {
		return
	}
	if err := os.WriteFile(path, []byte(version.String()+"\n"), 0o600); err != nil {
		c.log("save WhatsApp client version failed: %v", err)
	}
}

// recoverClientOutdated handles events.ClientOutdated. It runs on its own
// goroutine: it does network I/O and reconnects, neither of which may block
// whatsmeow's event handler.
func (c *client) recoverClientOutdated() {
	if c == nil {
		return
	}
	if !c.outdatedRecovering.CompareAndSwap(false, true) {
		return
	}
	defer c.outdatedRecovering.Store(false)

	rejected := store.GetWAVersion()
	c.log("WhatsApp rejected client version %s as outdated; fetching current version", rejected)
	emitEvent(bridgeEvent{
		Type:   "reconnecting",
		Reason: fmt.Sprintf("WhatsApp rejected client version %s as outdated, updating", rejected),
	})

	ctx, cancel := context.WithTimeout(context.Background(), waVersionFetchTimeout)
	latest, err := fetchLatestWAVersion(ctx)
	cancel()
	if err != nil {
		c.log("fetch current WhatsApp client version failed: %v", err)
		emitClientOutdatedFailure(rejected, fmt.Sprintf("the current version could not be fetched (%v)", err))
		return
	}
	if !waVersionNewer(latest, rejected) {
		c.log("current WhatsApp client version %s is not newer than rejected %s", latest, rejected)
		emitClientOutdatedFailure(rejected, fmt.Sprintf("web.whatsapp.com still reports %s", latest))
		return
	}
	if c.closed.Load() {
		return
	}

	store.SetWAVersion(latest)
	saveWAVersion(c, latest)
	c.log("retrying WhatsApp connection with client version %s (was %s)", latest, rejected)
	if err := reconnectWhatsApp(c); err != nil {
		c.log("reconnect after WhatsApp client version update failed: %v", err)
		emitEvent(bridgeEvent{
			Type:   "disconnected",
			Reason: fmt.Sprintf("reconnect with client version %s failed: %v", latest, err),
		})
	}
}

func emitClientOutdatedFailure(rejected store.WAVersionContainer, detail string) {
	emitEvent(bridgeEvent{
		Type:   "disconnected",
		Reason: fmt.Sprintf("WhatsApp rejected client version %s as outdated", rejected),
	})
	emitEvent(bridgeEvent{
		Type:     "notice",
		Title:    "WhatsApp stopped syncing",
		Message:  fmt.Sprintf("WhatsApp rejected this client version (%s) and %s. Update chat-cli and restart it.", rejected, detail),
		Severity: "system_alert",
	})
}

// permanentDisconnectEvents maps a whatsmeow permanent disconnect (one it
// will not auto-reconnect from) to the bridge events that tell the user why
// syncing stopped and what to do. ClientOutdated is handled separately by
// recoverClientOutdated.
func permanentDisconnectEvents(evt events.PermanentDisconnect) []bridgeEvent {
	switch v := evt.(type) {
	case *events.LoggedOut:
		return []bridgeEvent{
			{Type: "disconnected", Reason: "logged out"},
			{
				Type:     "notice",
				Title:    "WhatsApp unlinked this device",
				Message:  fmt.Sprintf("WhatsApp ended the chat-cli session (%s). Restart chat-cli and link it again from WhatsApp on your phone (Linked devices).", v.Reason),
				Severity: "system_alert",
			},
		}
	case *events.StreamReplaced:
		// Another client using the same session took over; the user
		// usually caused this themselves, so the reason alone is enough.
		return []bridgeEvent{{Type: "disconnected", Reason: "stream replaced"}}
	case *events.TemporaryBan:
		return []bridgeEvent{
			{Type: "disconnected", Reason: v.PermanentDisconnectDescription()},
			{
				Type:     "notice",
				Title:    "WhatsApp temporarily banned this account",
				Message:  fmt.Sprintf("WhatsApp refused the connection: %s. chat-cli will not receive messages until the ban ends.", v.String()),
				Severity: "system_alert",
			},
		}
	default:
		reason := evt.PermanentDisconnectDescription()
		if failure, ok := evt.(*events.ConnectFailure); ok && strings.TrimSpace(failure.Message) != "" {
			reason = fmt.Sprintf("%s (%s)", reason, strings.TrimSpace(failure.Message))
		}
		if refresh, ok := evt.(*events.CATRefreshError); ok && refresh.Error != nil {
			reason = fmt.Sprintf("%s: %v", reason, refresh.Error)
		}
		return []bridgeEvent{
			{Type: "disconnected", Reason: reason},
			{
				Type:     "notice",
				Title:    "WhatsApp stopped syncing",
				Message:  fmt.Sprintf("WhatsApp closed the connection and chat-cli will not reconnect automatically: %s. Restart chat-cli to try again.", reason),
				Severity: "system_alert",
			},
		}
	}
}
