package main

// caBLE encrypted EID (BLE advert) handling.
//
// The authenticator broadcasts a 20-byte advert: a 16-byte AES-encrypted EID
// plus a 4-byte truncated HMAC tag. The EID, once decrypted, carries the tunnel
// server routing information and the handshake nonce. Layout follows Chromium's
// device/fido/cable/eid.cc (FromComponents/ToComponents):
//
//	eid[0]      = 0x00                  (reserved; validates a correct decrypt)
//	eid[1:11]   = nonce                (10 bytes)
//	eid[11:14]  = routing ID           (3 bytes)
//	eid[14:16]  = tunnel domain (uint16 little-endian, encoded)
//
// NOTE: byte-level compatibility with WhatsApp's authenticator must be
// confirmed against a live/Chromium reference transcript (plan Risk 4). The
// crypto here is internally consistent (encrypt/decrypt round-trip) and matches
// the documented stock-caBLE layout.

import (
	"crypto/aes"
	"crypto/hmac"
	"crypto/sha256"
	"fmt"
)

const (
	eidRoutingIDSize = 3
	eidNonceSize     = 10
	eidTagSize       = 4
)

// eidComponents is the decoded content of a caBLE EID.
type eidComponents struct {
	RoutingID    []byte // 3 bytes
	TunnelDomain uint16 // encoded domain (index for small values)
	Nonce        []byte // 10 bytes
}

// decryptAdvert verifies and decrypts a 20-byte BLE advert using the 64-byte
// EID key (key[:32] = AES-256 key, key[32:64] = HMAC key). It returns the
// decoded components, or an error if the advert is not addressed to us.
func decryptAdvert(advert, eidKey []byte) (*eidComponents, error) {
	if len(advert) != bleAdvertSize {
		return nil, fmt.Errorf("advert length %d, want %d", len(advert), bleAdvertSize)
	}
	if len(eidKey) != eidKeySize {
		return nil, fmt.Errorf("eid key length %d, want %d", len(eidKey), eidKeySize)
	}
	ciphertext := advert[:eidSize]
	tag := advert[eidSize:]

	// Verify the truncated HMAC tag over the ciphertext.
	mac := hmac.New(sha256.New, eidKey[32:64])
	mac.Write(ciphertext)
	want := mac.Sum(nil)[:eidTagSize]
	if !hmac.Equal(tag, want) {
		return nil, fmt.Errorf("advert HMAC mismatch (not addressed to us)")
	}

	block, err := aes.NewCipher(eidKey[:32])
	if err != nil {
		return nil, fmt.Errorf("aes cipher: %w", err)
	}
	eid := make([]byte, eidSize)
	block.Decrypt(eid, ciphertext)

	if eid[0] != 0x00 {
		return nil, fmt.Errorf("decrypted EID reserved byte = %#x, want 0 (wrong key)", eid[0])
	}
	return &eidComponents{
		Nonce:        append([]byte(nil), eid[1:11]...),
		RoutingID:    append([]byte(nil), eid[11:14]...),
		TunnelDomain: uint16(eid[14]) | uint16(eid[15])<<8,
	}, nil
}

// plaintextEID reconstructs the 16-byte decrypted EID from its components,
// in the canonical Chromium layout reserved(1)·nonce(10)·routing(3)·domain(2).
// This exact byte string is the HKDF salt for the caBLE PSK derivation
// (Chromium: Derive(secret, decrypted_eid, kPSK)).
func (c *eidComponents) plaintextEID() []byte {
	eid := make([]byte, eidSize)
	copy(eid[1:11], c.Nonce)
	copy(eid[11:14], c.RoutingID)
	eid[14] = byte(c.TunnelDomain)
	eid[15] = byte(c.TunnelDomain >> 8)
	return eid
}

// encryptAdvert is the inverse of decryptAdvert, used by unit tests to build
// synthetic adverts. It is not used on the live client path.
func encryptAdvert(c *eidComponents, eidKey []byte) ([]byte, error) {
	if len(eidKey) != eidKeySize {
		return nil, fmt.Errorf("eid key length %d, want %d", len(eidKey), eidKeySize)
	}
	if len(c.RoutingID) != eidRoutingIDSize || len(c.Nonce) != eidNonceSize {
		return nil, fmt.Errorf("invalid EID components")
	}
	eid := make([]byte, eidSize)
	eid[0] = 0x00
	copy(eid[1:11], c.Nonce)
	copy(eid[11:14], c.RoutingID)
	eid[14] = byte(c.TunnelDomain)
	eid[15] = byte(c.TunnelDomain >> 8)

	block, err := aes.NewCipher(eidKey[:32])
	if err != nil {
		return nil, fmt.Errorf("aes cipher: %w", err)
	}
	ct := make([]byte, eidSize)
	block.Encrypt(ct, eid)

	mac := hmac.New(sha256.New, eidKey[32:64])
	mac.Write(ct)
	tag := mac.Sum(nil)[:eidTagSize]

	return append(ct, tag...), nil
}
