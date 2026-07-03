package main

import (
	"crypto/ecdsa"
	"crypto/elliptic"
	"crypto/sha256"
	"encoding/base64"
	"encoding/binary"
	"encoding/json"
	"math/big"
	"path/filepath"
	"testing"

	"go.mau.fi/whatsmeow/types"
)

func testPasskeyRequestOptions() *types.WebAuthnPublicKey {
	return &types.WebAuthnPublicKey{
		Challenge:        []byte("this-is-a-32-byte-challenge-xxxx"),
		Timeout:          600000,
		RelyingPartID:    "whatsapp.com",
		AllowCredentials: nil,
		UserVerification: "required",
	}
}

func TestBuildWebAuthnAssertionIsWellFormedAndVerifies(t *testing.T) {
	c := &client{dbPath: filepath.Join(t.TempDir(), "wa.db")}
	pub := testPasskeyRequestOptions()

	resp, err := c.buildWebAuthnAssertion(pub)
	if err != nil {
		t.Fatalf("build assertion: %v", err)
	}

	if resp.Type != "public-key" {
		t.Fatalf("expected type public-key, got %q", resp.Type)
	}
	if base64.RawURLEncoding.EncodeToString(resp.RawID) != resp.ID {
		t.Fatalf("id %q must be base64url of rawId", resp.ID)
	}

	// clientDataJSON must echo the challenge and carry the get ceremony type.
	var clientData struct {
		Type      string `json:"type"`
		Challenge string `json:"challenge"`
		Origin    string `json:"origin"`
	}
	if err := json.Unmarshal(resp.Response.ClientDataJSON, &clientData); err != nil {
		t.Fatalf("clientDataJSON not valid json: %v", err)
	}
	if clientData.Type != "webauthn.get" {
		t.Fatalf("expected webauthn.get, got %q", clientData.Type)
	}
	if clientData.Challenge != base64.RawURLEncoding.EncodeToString(pub.Challenge) {
		t.Fatalf("challenge not echoed correctly: %q", clientData.Challenge)
	}
	if clientData.Origin != "https://web.whatsapp.com" {
		t.Fatalf("unexpected origin %q", clientData.Origin)
	}

	// authenticatorData: rpIdHash || flags || signCount(=1 on first use).
	authData := resp.Response.AuthenticatorData
	if len(authData) != 37 {
		t.Fatalf("expected 37-byte authenticatorData, got %d", len(authData))
	}
	rpHash := sha256.Sum256([]byte("whatsapp.com"))
	if string(authData[:32]) != string(rpHash[:]) {
		t.Fatal("rpIdHash mismatch in authenticatorData")
	}
	if authData[32] != 0x05 {
		t.Fatalf("expected UP|UV flags 0x05, got %#x", authData[32])
	}
	if got := binary.BigEndian.Uint32(authData[33:37]); got != 1 {
		t.Fatalf("expected sign count 1 on first use, got %d", got)
	}

	// The signature must verify against the persisted public key.
	cred, _, err := c.loadOrCreatePasskey()
	if err != nil {
		t.Fatalf("reload passkey: %v", err)
	}
	pubKey := &ecdsa.PublicKey{
		Curve: elliptic.P256(),
		X:     new(big.Int).SetBytes(cred.PublicKeyX),
		Y:     new(big.Int).SetBytes(cred.PublicKeyY),
	}
	clientDataHash := sha256.Sum256(resp.Response.ClientDataJSON)
	signed := append(append([]byte{}, authData...), clientDataHash[:]...)
	digest := sha256.Sum256(signed)
	if !ecdsa.VerifyASN1(pubKey, digest[:], resp.Response.Signature) {
		t.Fatal("assertion signature failed to verify against stored public key")
	}
}

func TestPasskeyCredentialPersistsAndCounterIncrements(t *testing.T) {
	c := &client{dbPath: filepath.Join(t.TempDir(), "wa.db")}
	pub := testPasskeyRequestOptions()

	first, err := c.buildWebAuthnAssertion(pub)
	if err != nil {
		t.Fatalf("first assertion: %v", err)
	}
	second, err := c.buildWebAuthnAssertion(pub)
	if err != nil {
		t.Fatalf("second assertion: %v", err)
	}

	if string(first.RawID) != string(second.RawID) {
		t.Fatal("credential id must be stable across assertions")
	}
	firstCount := binary.BigEndian.Uint32(first.Response.AuthenticatorData[33:37])
	secondCount := binary.BigEndian.Uint32(second.Response.AuthenticatorData[33:37])
	if secondCount != firstCount+1 {
		t.Fatalf("sign counter must increment: first=%d second=%d", firstCount, secondCount)
	}
}
