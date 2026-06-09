package main

import (
	"testing"

	"go.mau.fi/whatsmeow/types"
)

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
