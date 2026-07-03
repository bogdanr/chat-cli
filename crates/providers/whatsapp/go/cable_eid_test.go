package main

import (
	"bytes"
	"crypto/aes"
	"crypto/hmac"
	"crypto/rand"
	"crypto/sha256"
	"testing"
)

// TestEIDAdvertFieldOffsets pins the absolute EID byte layout
// (reserved · nonce[10] · routing[3] · domain[2]) against Chromium's eid.cc.
// A round-trip test cannot catch a layout bug because encrypt/decrypt are
// symmetric; this builds the raw plaintext by hand so a wrong offset fails.
func TestEIDAdvertFieldOffsets(t *testing.T) {
	eidKey := make([]byte, eidKeySize)
	for i := range eidKey {
		eidKey[i] = byte(i)
	}

	// reserved=0, nonce=0xA0..0xA9, routing=0xB0 0xB1 0xB2, domain=0x0000 (=0).
	eid := make([]byte, eidSize)
	nonce := []byte{0xA0, 0xA1, 0xA2, 0xA3, 0xA4, 0xA5, 0xA6, 0xA7, 0xA8, 0xA9}
	routing := []byte{0xB0, 0xB1, 0xB2}
	copy(eid[1:11], nonce)
	copy(eid[11:14], routing)
	// eid[14:16] left zero => domain 0 (cable.ua5v.com), matching the live
	// WhatsApp advert whose real domain was 0.

	block, err := aes.NewCipher(eidKey[:32])
	if err != nil {
		t.Fatalf("aes: %v", err)
	}
	ct := make([]byte, eidSize)
	block.Encrypt(ct, eid)
	mac := hmac.New(sha256.New, eidKey[32:64])
	mac.Write(ct)
	advert := append(ct, mac.Sum(nil)[:eidTagSize]...)

	got, err := decryptAdvert(advert, eidKey)
	if err != nil {
		t.Fatalf("decryptAdvert: %v", err)
	}
	if !bytes.Equal(got.Nonce, nonce) {
		t.Errorf("nonce = %x, want %x", got.Nonce, nonce)
	}
	if !bytes.Equal(got.RoutingID, routing) {
		t.Errorf("routing = %x, want %x", got.RoutingID, routing)
	}
	if got.TunnelDomain != 0 {
		t.Errorf("domain = %d, want 0", got.TunnelDomain)
	}
	if domain, derr := decodeTunnelDomain(got.TunnelDomain); derr != nil || domain != "cable.ua5v.com" {
		t.Errorf("decodeTunnelDomain(0) = %q, %v; want cable.ua5v.com", domain, derr)
	}
}

func TestEIDAdvertRoundTrip(t *testing.T) {
	eidKey := make([]byte, eidKeySize)
	if _, err := rand.Read(eidKey); err != nil {
		t.Fatalf("rand: %v", err)
	}
	want := &eidComponents{
		RoutingID:    []byte{0x11, 0x22, 0x33},
		TunnelDomain: 1,
		Nonce:        []byte{1, 2, 3, 4, 5, 6, 7, 8, 9, 10},
	}
	advert, err := encryptAdvert(want, eidKey)
	if err != nil {
		t.Fatalf("encryptAdvert: %v", err)
	}
	if len(advert) != bleAdvertSize {
		t.Fatalf("advert len %d, want %d", len(advert), bleAdvertSize)
	}
	got, err := decryptAdvert(advert, eidKey)
	if err != nil {
		t.Fatalf("decryptAdvert: %v", err)
	}
	if !bytes.Equal(got.RoutingID, want.RoutingID) || got.TunnelDomain != want.TunnelDomain || !bytes.Equal(got.Nonce, want.Nonce) {
		t.Fatalf("round-trip mismatch: got %+v want %+v", got, want)
	}
}

func TestEIDAdvertWrongKeyRejected(t *testing.T) {
	eidKey := make([]byte, eidKeySize)
	rand.Read(eidKey)
	other := make([]byte, eidKeySize)
	rand.Read(other)

	advert, err := encryptAdvert(&eidComponents{
		RoutingID: []byte{1, 2, 3},
		Nonce:     make([]byte, 10),
	}, eidKey)
	if err != nil {
		t.Fatalf("encryptAdvert: %v", err)
	}
	if _, err := decryptAdvert(advert, other); err == nil {
		t.Fatalf("expected HMAC mismatch with wrong key, got nil")
	}
}

func TestCableDeriveDeterministic(t *testing.T) {
	secret := bytes.Repeat([]byte{0xab}, 16)
	nonce := bytes.Repeat([]byte{0xcd}, 10)
	a, err := cableDerive(secret, nonce, derivePSK, pskSize)
	if err != nil {
		t.Fatalf("derive: %v", err)
	}
	b, _ := cableDerive(secret, nonce, derivePSK, pskSize)
	if !bytes.Equal(a, b) {
		t.Fatalf("derive not deterministic")
	}
	// Different type must produce a different key.
	c, _ := cableDerive(secret, nonce, deriveTunnelID, pskSize)
	if bytes.Equal(a, c) {
		t.Fatalf("different value types produced identical keys")
	}
	if len(a) != pskSize {
		t.Fatalf("len %d want %d", len(a), pskSize)
	}
}
