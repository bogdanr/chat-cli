package main

import "testing"

// TestDecodeTunnelDomainAssigned verifies the small assigned-table values map to
// the two well-known caBLE tunnel hosts.
func TestDecodeTunnelDomainAssigned(t *testing.T) {
	cases := map[uint16]string{
		0: "cable.ua5v.com",
		1: "cable.auth.com",
	}
	for encoded, want := range cases {
		got, err := decodeTunnelDomain(encoded)
		if err != nil {
			t.Fatalf("decodeTunnelDomain(%d) error: %v", encoded, err)
		}
		if got != want {
			t.Errorf("decodeTunnelDomain(%d) = %q, want %q", encoded, got, want)
		}
	}
}

// TestDecodeTunnelDomainUnassigned covers the invalid [len(assigned),256) gap.
func TestDecodeTunnelDomainUnassigned(t *testing.T) {
	if _, err := decodeTunnelDomain(2); err == nil {
		t.Fatalf("decodeTunnelDomain(2) expected error for unassigned value")
	}
}

// TestDecodeTunnelDomainHashed pins the hashed derivation for values >=256
// against Chromium's tunnelserver::DecodeDomain. Note: in practice WhatsApp's
// Android/iOS authenticator advertises assigned domain 0 (cable.ua5v.com), so
// the hashed path is a correctness safety net rather than the live path.
func TestDecodeTunnelDomainHashed(t *testing.T) {
	cases := map[uint16]string{
		256:   "cable.qz2ekwmnd332c.info",
		266:   "cable.wufkweyy3uaxb.com",
		14999: "cable.d4o5w3ujg2o.info",
	}
	for encoded, want := range cases {
		got, err := decodeTunnelDomain(encoded)
		if err != nil {
			t.Fatalf("decodeTunnelDomain(%d) error: %v", encoded, err)
		}
		if got != want {
			t.Errorf("decodeTunnelDomain(%d) = %q, want %q", encoded, got, want)
		}
	}
}

// TestToHexUppercase guards the rendezvous encoding: the tunnel id must be
// uppercase hex to match Chromium/base::HexEncode, since both peers derive the
// same id and the tunnel server pairs them by the URL path.
func TestToHexUppercase(t *testing.T) {
	if got := toHex([]byte{0x9e, 0xd3, 0xc4, 0x0f}); got != "9ED3C40F" {
		t.Errorf("toHex = %q, want %q", got, "9ED3C40F")
	}
}
