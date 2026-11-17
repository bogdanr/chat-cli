package main

import (
	"context"
	"errors"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"testing"
	"time"

	"go.mau.fi/whatsmeow/store"
	"go.mau.fi/whatsmeow/types/events"
)

// recoveryHarness stubs the network, reconnect and emit hooks used by
// ClientOutdated recovery, and restores the process-wide WA version.
type recoveryHarness struct {
	mu         sync.Mutex
	events     []bridgeEvent
	reconnects []store.WAVersionContainer
}

func newRecoveryHarness(t *testing.T, latest store.WAVersionContainer, fetchErr error) *recoveryHarness {
	t.Helper()
	h := &recoveryHarness{}
	original := store.GetWAVersion()
	prevFetch, prevReconnect, prevEmit := fetchLatestWAVersion, reconnectWhatsApp, emitEvent
	t.Cleanup(func() {
		store.SetWAVersion(original)
		fetchLatestWAVersion, reconnectWhatsApp, emitEvent = prevFetch, prevReconnect, prevEmit
	})
	fetchLatestWAVersion = func(context.Context) (store.WAVersionContainer, error) {
		return latest, fetchErr
	}
	reconnectWhatsApp = func(*client) error {
		h.mu.Lock()
		defer h.mu.Unlock()
		h.reconnects = append(h.reconnects, store.GetWAVersion())
		return nil
	}
	emitEvent = func(evt bridgeEvent) {
		h.mu.Lock()
		defer h.mu.Unlock()
		h.events = append(h.events, evt)
	}
	return h
}

func (h *recoveryHarness) types() []string {
	h.mu.Lock()
	defer h.mu.Unlock()
	out := make([]string, 0, len(h.events))
	for _, evt := range h.events {
		out = append(out, evt.Type)
	}
	return out
}

func TestParseWAVersion(t *testing.T) {
	got, ok := parseWAVersion(" 2.3000.1054933766\n")
	if !ok || got != (store.WAVersionContainer{2, 3000, 1054933766}) {
		t.Fatalf("parseWAVersion = %v, %v", got, ok)
	}
	for _, bad := range []string{"", "2.3000", "2.3000.x", "0.0.0", "2.3000.1.4", "-1.2.3"} {
		if _, ok := parseWAVersion(bad); ok {
			t.Fatalf("parseWAVersion(%q) should fail", bad)
		}
	}
}

func TestWAVersionNewerComparesComponentsInOrder(t *testing.T) {
	base := store.WAVersionContainer{2, 3000, 1042386815}
	if !waVersionNewer(store.WAVersionContainer{2, 3000, 1054933766}, base) {
		t.Fatal("higher tertiary must be newer")
	}
	if !waVersionNewer(store.WAVersionContainer{3, 0, 0}, base) {
		t.Fatal("higher primary must win over lower tertiary")
	}
	if waVersionNewer(base, base) {
		t.Fatal("equal versions are not newer")
	}
	if waVersionNewer(store.WAVersionContainer{2, 2999, 2000000000}, base) {
		t.Fatal("lower secondary must not be newer")
	}
}

func TestWAVersionCachePathSkipsMemoryAndTestStores(t *testing.T) {
	for _, path := range []string{"", ":memory:", "test:abc"} {
		if got := waVersionCachePath(path); got != "" {
			t.Fatalf("waVersionCachePath(%q) = %q, want empty", path, got)
		}
	}
	if got := waVersionCachePath("/data/whatsapp.db"); got != "/data/whatsapp.db.waversion" {
		t.Fatalf("unexpected cache path %q", got)
	}
}

func TestApplyCachedWAVersionOnlyMovesForward(t *testing.T) {
	original := store.GetWAVersion()
	t.Cleanup(func() { store.SetWAVersion(original) })
	dbPath := filepath.Join(t.TempDir(), "whatsapp.db")
	c := &client{dbPath: dbPath}

	newer := store.WAVersionContainer{original[0], original[1], original[2] + 10}
	if err := os.WriteFile(waVersionCachePath(dbPath), []byte(newer.String()+"\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	applyCachedWAVersion(c)
	if store.GetWAVersion() != newer {
		t.Fatalf("cached newer version not applied: %s", store.GetWAVersion())
	}

	store.SetWAVersion(original)
	older := store.WAVersionContainer{original[0], original[1], original[2] - 10}
	if err := os.WriteFile(waVersionCachePath(dbPath), []byte(older.String()), 0o600); err != nil {
		t.Fatal(err)
	}
	applyCachedWAVersion(c)
	if store.GetWAVersion() != original {
		t.Fatalf("older cached version must not override built-in: %s", store.GetWAVersion())
	}
}

func TestRecoverClientOutdatedUpdatesVersionSavesItAndReconnects(t *testing.T) {
	rejected := store.GetWAVersion()
	latest := store.WAVersionContainer{rejected[0], rejected[1], rejected[2] + 12547}
	h := newRecoveryHarness(t, latest, nil)
	dbPath := filepath.Join(t.TempDir(), "whatsapp.db")
	c := &client{dbPath: dbPath}

	c.recoverClientOutdated()

	if store.GetWAVersion() != latest {
		t.Fatalf("version not updated: %s", store.GetWAVersion())
	}
	if len(h.reconnects) != 1 || h.reconnects[0] != latest {
		t.Fatalf("expected one reconnect with %s, got %v", latest, h.reconnects)
	}
	saved, err := os.ReadFile(waVersionCachePath(dbPath))
	if err != nil || strings.TrimSpace(string(saved)) != latest.String() {
		t.Fatalf("saved version = %q, %v", saved, err)
	}
	if got := h.types(); len(got) != 1 || got[0] != "reconnecting" {
		t.Fatalf("expected only a reconnecting event, got %v", got)
	}
	if !strings.Contains(h.events[0].Reason, rejected.String()) {
		t.Fatalf("reconnecting reason should name the rejected version: %q", h.events[0].Reason)
	}
}

func TestRecoverClientOutdatedAlertsWhenFetchFails(t *testing.T) {
	rejected := store.GetWAVersion()
	h := newRecoveryHarness(t, store.WAVersionContainer{}, errors.New("dial tcp: no route"))
	c := &client{dbPath: filepath.Join(t.TempDir(), "whatsapp.db")}

	c.recoverClientOutdated()

	if len(h.reconnects) != 0 {
		t.Fatalf("must not reconnect without a newer version, got %v", h.reconnects)
	}
	if store.GetWAVersion() != rejected {
		t.Fatal("version must be unchanged after a failed fetch")
	}
	got := h.types()
	if len(got) != 3 || got[0] != "reconnecting" || got[1] != "disconnected" || got[2] != "notice" {
		t.Fatalf("expected reconnecting, disconnected, notice; got %v", got)
	}
	notice := h.events[2]
	if notice.Severity != "system_alert" || !strings.Contains(notice.Message, "no route") {
		t.Fatalf("notice should be a system alert carrying the error: %+v", notice)
	}
}

func TestRecoverClientOutdatedDoesNotLoopOnSameVersion(t *testing.T) {
	rejected := store.GetWAVersion()
	h := newRecoveryHarness(t, rejected, nil)
	c := &client{dbPath: filepath.Join(t.TempDir(), "whatsapp.db")}

	c.recoverClientOutdated()

	if len(h.reconnects) != 0 {
		t.Fatalf("same version must not trigger a reconnect, got %v", h.reconnects)
	}
	if got := h.types(); len(got) != 3 || got[2] != "notice" {
		t.Fatalf("expected a failure notice, got %v", got)
	}
}

func TestRecoverClientOutdatedSkipsReconnectForClosedClient(t *testing.T) {
	rejected := store.GetWAVersion()
	h := newRecoveryHarness(t, store.WAVersionContainer{rejected[0], rejected[1], rejected[2] + 1}, nil)
	c := &client{dbPath: filepath.Join(t.TempDir(), "whatsapp.db")}
	c.closed.Store(true)

	c.recoverClientOutdated()

	if len(h.reconnects) != 0 {
		t.Fatalf("closed client must not reconnect, got %v", h.reconnects)
	}
}

func TestClientOutdatedEventStartsRecovery(t *testing.T) {
	rejected := store.GetWAVersion()
	h := newRecoveryHarness(t, store.WAVersionContainer{rejected[0], rejected[1], rejected[2] + 1}, nil)
	c := &client{dbPath: filepath.Join(t.TempDir(), "whatsapp.db")}

	handleWhatsAppEvent(c, &events.ClientOutdated{})

	deadline := time.Now().Add(2 * time.Second)
	for {
		h.mu.Lock()
		n := len(h.reconnects)
		h.mu.Unlock()
		if n == 1 {
			break
		}
		if time.Now().After(deadline) {
			t.Fatal("ClientOutdated did not trigger a reconnect")
		}
		time.Sleep(10 * time.Millisecond)
	}
}

func TestPermanentDisconnectEventsExplainEveryCause(t *testing.T) {
	cases := []struct {
		name       string
		evt        events.PermanentDisconnect
		wantReason string
		wantNotice bool
	}{
		{"logged out", &events.LoggedOut{OnConnect: true, Reason: events.ConnectFailureLoggedOut}, "logged out", true},
		{"stream replaced", &events.StreamReplaced{}, "stream replaced", false},
		{"temp ban", &events.TemporaryBan{Code: events.TempBanSentToTooManyPeople, Expire: time.Hour}, "temporarily banned", true},
		{"connect failure", &events.ConnectFailure{Reason: events.ConnectFailureServiceUnavailable, Message: "busy"}, "busy", true},
		{"cat refresh", &events.CATRefreshError{Error: errors.New("token rejected")}, "token rejected", true},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			out := permanentDisconnectEvents(tc.evt)
			if len(out) == 0 || out[0].Type != "disconnected" || !strings.Contains(out[0].Reason, tc.wantReason) {
				t.Fatalf("first event should be disconnected with %q, got %+v", tc.wantReason, out)
			}
			hasNotice := len(out) == 2 && out[1].Type == "notice" && out[1].Severity == "system_alert" &&
				out[1].Title != "" && out[1].Message != ""
			if hasNotice != tc.wantNotice {
				t.Fatalf("notice present=%v, want %v: %+v", hasNotice, tc.wantNotice, out)
			}
		})
	}
}
