package main

import (
	"context"
	"errors"
	"sync"
	"sync/atomic"
	"testing"
	"time"

	"go.mau.fi/whatsmeow/types"
)

func stubGroupNameLookup(t *testing.T, fn func(types.JID) (string, error)) *atomic.Int32 {
	t.Helper()
	calls := &atomic.Int32{}
	prev := groupNameLookup
	t.Cleanup(func() { groupNameLookup = prev })
	groupNameLookup = func(_ *client, _ context.Context, jid types.JID) (string, error) {
		calls.Add(1)
		return fn(jid)
	}
	return calls
}

func stubGroupNow(t *testing.T, start time.Time) *time.Time {
	t.Helper()
	now := start
	prev := groupNow
	t.Cleanup(func() { groupNow = prev })
	groupNow = func() time.Time { return now }
	return &now
}

var testGroup = types.NewJID("120363028315852690", types.GroupServer)

func TestConversationNameNeverReturnsRawGroupIDOnLookupFailure(t *testing.T) {
	stubGroupNameLookup(t, func(types.JID) (string, error) {
		return "", errors.New("429 rate-overlimit")
	})
	c := &client{}
	if got := conversationName(c, context.Background(), testGroup, ""); got != "" {
		t.Fatalf("failed lookup must yield an empty name, got %q", got)
	}
}

func TestConversationNamePrefersCacheThenFallbackBeforeNetwork(t *testing.T) {
	calls := stubGroupNameLookup(t, func(types.JID) (string, error) { return "From network", nil })
	c := &client{}

	if got := conversationName(c, context.Background(), testGroup, "From history"); got != "From history" {
		t.Fatalf("fallback should be used before a network lookup, got %q", got)
	}
	if calls.Load() != 0 {
		t.Fatalf("no lookup expected when a fallback exists, got %d", calls.Load())
	}

	c.rememberGroupName(testGroup, "Învățat")
	if got := conversationName(c, context.Background(), testGroup, "From history"); got != "Învățat" {
		t.Fatalf("cached subject should win, got %q", got)
	}
	if calls.Load() != 0 {
		t.Fatalf("cache hit must not query, got %d", calls.Load())
	}
}

func TestGroupNameCachesSuccessfulLookup(t *testing.T) {
	calls := stubGroupNameLookup(t, func(types.JID) (string, error) { return "Învățat", nil })
	c := &client{}
	for i := 0; i < 50; i++ {
		if got := c.groupName(context.Background(), testGroup); got != "Învățat" {
			t.Fatalf("lookup %d = %q", i, got)
		}
	}
	if calls.Load() != 1 {
		t.Fatalf("expected one network lookup for 50 messages, got %d", calls.Load())
	}
}

func TestGroupNameBacksOffAfterFailure(t *testing.T) {
	fail := true
	calls := stubGroupNameLookup(t, func(types.JID) (string, error) {
		if fail {
			return "", errors.New("429 rate-overlimit")
		}
		return "Învățat", nil
	})
	now := stubGroupNow(t, time.Date(2026, 11, 17, 10, 0, 0, 0, time.UTC))
	c := &client{}

	for i := 0; i < 20; i++ {
		if got := c.groupName(context.Background(), testGroup); got != "" {
			t.Fatalf("expected empty name while failing, got %q", got)
		}
	}
	if calls.Load() != 1 {
		t.Fatalf("failed group must not be re-queried during backoff, got %d lookups", calls.Load())
	}

	fail = false
	*now = now.Add(groupNameRetryBackoff + time.Second)
	if got := c.groupName(context.Background(), testGroup); got != "Învățat" {
		t.Fatalf("lookup after backoff = %q", got)
	}
	if calls.Load() != 2 {
		t.Fatalf("expected a retry after backoff, got %d lookups", calls.Load())
	}
}

func TestGroupNameDeduplicatesConcurrentLookups(t *testing.T) {
	release := make(chan struct{})
	started := make(chan struct{}, 1)
	calls := stubGroupNameLookup(t, func(types.JID) (string, error) {
		started <- struct{}{}
		<-release
		return "Învățat", nil
	})
	c := &client{}

	var wg sync.WaitGroup
	wg.Add(1)
	go func() {
		defer wg.Done()
		c.groupName(context.Background(), testGroup)
	}()
	<-started
	for i := 0; i < 10; i++ {
		if got := c.groupName(context.Background(), testGroup); got != "" {
			t.Fatalf("concurrent caller should not wait or query, got %q", got)
		}
	}
	close(release)
	wg.Wait()
	if calls.Load() != 1 {
		t.Fatalf("expected a single in-flight lookup, got %d", calls.Load())
	}
	if got := c.cachedGroupName(testGroup); got != "Învățat" {
		t.Fatalf("result should be cached, got %q", got)
	}
}

func TestRememberGroupNameIgnoresEmptyAndClearsBackoff(t *testing.T) {
	calls := stubGroupNameLookup(t, func(types.JID) (string, error) { return "", errors.New("down") })
	c := &client{}
	c.groupName(context.Background(), testGroup)
	c.rememberGroupName(testGroup, "")
	if got := c.cachedGroupName(testGroup); got != "" {
		t.Fatalf("empty name must not be cached, got %q", got)
	}
	c.rememberGroupName(testGroup, "Învățat")
	if got := c.groupName(context.Background(), testGroup); got != "Învățat" {
		t.Fatalf("remembered name should be served, got %q", got)
	}
	if calls.Load() != 1 {
		t.Fatalf("expected only the initial failed lookup, got %d", calls.Load())
	}
}
