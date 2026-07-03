package main

import (
	"bytes"
	"encoding/json"
	"testing"

	"github.com/fxamacker/cbor/v2"
	"go.mau.fi/whatsmeow/types"
)

func TestBuildGetAssertionCommand(t *testing.T) {
	pub := &types.WebAuthnPublicKey{
		Challenge:        []byte{1, 2, 3, 4},
		RelyingPartID:    "whatsapp.com",
		UserVerification: "required",
	}
	cdh := sha256Sum([]byte("clientdata"))
	cmd, err := buildGetAssertionCommand(pub, cdh)
	if err != nil {
		t.Fatalf("build: %v", err)
	}
	if cmd[0] != ctapCmdGetAssertion {
		t.Fatalf("command byte %#x, want %#x", cmd[0], ctapCmdGetAssertion)
	}
	var m map[int]cbor.RawMessage
	if err := cbor.Unmarshal(cmd[1:], &m); err != nil {
		t.Fatalf("decode command: %v", err)
	}
	var rpID string
	if err := cbor.Unmarshal(m[1], &rpID); err != nil || rpID != "whatsapp.com" {
		t.Fatalf("rpId = %q err %v", rpID, err)
	}
	var gotCDH []byte
	if err := cbor.Unmarshal(m[2], &gotCDH); err != nil || !bytes.Equal(gotCDH, cdh) {
		t.Fatalf("clientDataHash mismatch")
	}
	if _, ok := m[5]; !ok {
		t.Fatalf("expected options map (uv) present")
	}
}

func TestParseGetAssertionResponseAndMap(t *testing.T) {
	credID := []byte{0xaa, 0xbb, 0xcc}
	authData := bytes.Repeat([]byte{0x37}, 37)
	sig := []byte{0xde, 0xad, 0xbe, 0xef}
	userID := []byte{0x01, 0x02}

	respMap := map[int]any{
		1: map[string]any{"type": "public-key", "id": credID},
		2: authData,
		3: sig,
		4: map[string]any{"id": userID},
		5: 1,
	}
	encMode, _ := cbor.CTAP2EncOptions().EncMode()
	body, err := encMode.Marshal(respMap)
	if err != nil {
		t.Fatalf("marshal resp: %v", err)
	}
	full := append([]byte{0x00}, body...)

	parsed, err := parseGetAssertionResponse(full)
	if err != nil {
		t.Fatalf("parse: %v", err)
	}
	if !bytes.Equal(parsed.CredentialID, credID) || !bytes.Equal(parsed.AuthenticatorData, authData) ||
		!bytes.Equal(parsed.Signature, sig) || !bytes.Equal(parsed.UserHandle, userID) {
		t.Fatalf("parsed fields mismatch: %+v", parsed)
	}

	// clientDataJSON byte-identity through the mapping.
	cdj, _ := buildClientDataJSON([]byte("challenge"), "https://web.whatsapp.com")
	war := parsed.toWebAuthnResponse(cdj)
	if !bytes.Equal(war.Response.ClientDataJSON, cdj) {
		t.Fatalf("clientDataJSON not preserved through mapping")
	}
	if war.Response.UserHandle == nil || !bytes.Equal(*war.Response.UserHandle, userID) {
		t.Fatalf("userHandle not mapped")
	}
	// Ensure the clientDataJSON is valid JSON of the expected shape.
	var check map[string]any
	if err := json.Unmarshal(cdj, &check); err != nil || check["type"] != "webauthn.get" {
		t.Fatalf("clientDataJSON invalid: %v", err)
	}
}

func TestParseGetAssertionErrorStatus(t *testing.T) {
	if _, err := parseGetAssertionResponse([]byte{0x27}); err == nil {
		t.Fatalf("expected error for non-zero CTAP status")
	}
}
