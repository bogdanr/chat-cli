package main

import (
	"context"
	"fmt"
	"sync"
	"time"

	"go.mau.fi/whatsmeow/types"
)

// Group names used to be looked up with a live GetGroupInfo query for every
// incoming group message. During an offline-backlog burst that is dozens of
// queries per second; WhatsApp answers 429 rate-overlimit, and the bridge
// then fell back to the raw group ID as the chat name, which overwrote the
// real name in storage (the chat became unsearchable by name).
//
// groupNameCache keeps the last known subject per group, seeded from the
// joined-groups list on connect, group-info results and rename events. A
// cache miss triggers at most one lookup per group at a time, and a failed
// lookup backs off before that group is queried again. When no name is
// known the bridge sends an empty name, never the ID.
type groupNameCache struct {
	mu       sync.Mutex
	names    map[string]string
	inflight map[string]bool
	retryAt  map[string]time.Time
}

// groupNameRetryBackoff is how long a group is not queried again after a
// failed lookup (rate limit, network error, not a participant).
const groupNameRetryBackoff = 2 * time.Minute

// groupNameLookup is a package variable so tests can stub the network.
var groupNameLookup = func(c *client, ctx context.Context, jid types.JID) (string, error) {
	if c == nil || c.wa == nil {
		return "", fmt.Errorf("WhatsApp client not initialized")
	}
	info, err := c.wa.GetGroupInfo(ctx, jid)
	if err != nil {
		return "", err
	}
	if info == nil {
		return "", fmt.Errorf("empty group info")
	}
	return info.Name, nil
}

// groupNow is a package variable so tests can control backoff timing.
var groupNow = time.Now

func (g *groupNameCache) lockedInit() {
	if g.names == nil {
		g.names = make(map[string]string)
		g.inflight = make(map[string]bool)
		g.retryAt = make(map[string]time.Time)
	}
}

// rememberGroupName records an authoritative group subject. Empty names are
// ignored so a partial event never erases a known name.
func (c *client) rememberGroupName(jid types.JID, name string) {
	if c == nil || jid.IsEmpty() || name == "" {
		return
	}
	g := &c.groupNames
	g.mu.Lock()
	defer g.mu.Unlock()
	g.lockedInit()
	key := jid.ToNonAD().String()
	g.names[key] = name
	delete(g.retryAt, key)
}

func (c *client) cachedGroupName(jid types.JID) string {
	if c == nil || jid.IsEmpty() {
		return ""
	}
	g := &c.groupNames
	g.mu.Lock()
	defer g.mu.Unlock()
	g.lockedInit()
	return g.names[jid.ToNonAD().String()]
}

// groupName returns the cached subject, or performs a single deduplicated
// lookup on a miss. It returns "" when the name is unknown, a lookup for the
// group is already running, or the group is backing off after a failure.
func (c *client) groupName(ctx context.Context, jid types.JID) string {
	if c == nil || jid.IsEmpty() || jid.Server != types.GroupServer {
		return ""
	}
	key := jid.ToNonAD().String()
	g := &c.groupNames
	g.mu.Lock()
	g.lockedInit()
	if name := g.names[key]; name != "" {
		g.mu.Unlock()
		return name
	}
	if g.inflight[key] || groupNow().Before(g.retryAt[key]) {
		g.mu.Unlock()
		return ""
	}
	g.inflight[key] = true
	g.mu.Unlock()

	name, err := groupNameLookup(c, ctx, jid)

	g.mu.Lock()
	delete(g.inflight, key)
	if err != nil || name == "" {
		g.retryAt[key] = groupNow().Add(groupNameRetryBackoff)
		g.mu.Unlock()
		if err != nil {
			c.log("fetch WhatsApp group name failed chat=%s retry_in=%s: %v", key, groupNameRetryBackoff, err)
		}
		return ""
	}
	g.names[key] = name
	delete(g.retryAt, key)
	g.mu.Unlock()
	return name
}
