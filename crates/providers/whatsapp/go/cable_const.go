package main

// Centralised caBLE / CTAP 2.2 hybrid constants and key-derivation helpers.
//
// These follow the FIDO caBLE v2 / CTAP 2.2 hybrid spec and Chromium's
// device/fido/cable implementation. They are gathered in one file because the
// most likely place WhatsApp's variant deviates from stock caBLE is a salt,
// info string, or field offset (see the implementation plan, Risk 4). Keeping
// them together makes a live-transcript-driven correction a one-file change.

import (
	"crypto/rand"
	"crypto/sha256"
	"fmt"
	"io"

	"golang.org/x/crypto/hkdf"
)

// rngReader is the entropy source for caBLE key generation. It is a package
// variable so unit tests can inject deterministic keys for handshake vectors.
var rngReader io.Reader = rand.Reader

// derivedValueType enumerates the HKDF "info" selectors used by caBLE. Values
// match Chromium's DerivedValueType so the derivations are wire-compatible.
type derivedValueType uint32

const (
	deriveEIDKey         derivedValueType = 1
	deriveTunnelID       derivedValueType = 2
	derivePSK            derivedValueType = 3
	deriveIdentityKeySeed derivedValueType = 5
)

// Derived key sizes.
const (
	eidKeySize   = 64 // 32-byte AES key + 32-byte HMAC key
	tunnelIDSize = 16
	pskSize      = 32
	// bleAdvertSize is the on-air size of the authenticator's caBLE advert:
	// a 16-byte encrypted EID plus a 4-byte HMAC tag.
	bleAdvertSize = 20
	eidSize       = 16
)

// FIDO caBLE BLE service data UUID (16-bit): 0xFFF9.
const cableServiceUUID16 = 0xFFF9

// cableWebSocketSubprotocol is required by the tunnel servers during upgrade.
const cableWebSocketSubprotocol = "fido.cable"

// cableDerive runs HKDF-SHA256 with the caBLE convention: IKM = secret,
// salt = nonce (may be empty), info = little-endian uint32 of the value type.
func cableDerive(secret, nonce []byte, typ derivedValueType, outLen int) ([]byte, error) {
	info := []byte{
		byte(typ),
		byte(typ >> 8),
		byte(typ >> 16),
		byte(typ >> 24),
	}
	r := hkdf.New(sha256.New, secret, nonce, info)
	out := make([]byte, outLen)
	if _, err := io.ReadFull(r, out); err != nil {
		return nil, fmt.Errorf("caBLE HKDF derive (type=%d): %w", typ, err)
	}
	return out, nil
}
