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
