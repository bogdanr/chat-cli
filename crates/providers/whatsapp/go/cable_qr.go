package main

// caBLE / CTAP 2.2 "hybrid transport" QR generation (client/initiator role).
//
// In the hybrid ceremony chat-cli is the WebAuthn *client*: it publishes a
// `FIDO:/` QR that the phone (the authenticator) scans. The QR carries an
// ephemeral P-256 public key and a 16-byte secret from which every subsequent
// key (BLE advert decryption, Noise PSK) is derived. See CTAP 2.2 §11.5.3.
//
// The exact structure below follows the CTAP 2.2 hybrid spec and Chromium's
// device/fido/cable implementation. Constants that WhatsApp could conceivably
// diverge on are centralised in cable_const.go for quick live adjustment.

import (
	"crypto/ecdh"
	"crypto/rand"
	"fmt"
	"math/big"
	"time"

	"github.com/fxamacker/cbor/v2"
)

// cableQR holds the ephemeral client state that seeds the whole ceremony. The
// private key is kept to run the later Noise handshake; the QR secret derives
// the BLE advert key and the handshake PSK.
type cableQR struct {
	privKey   *ecdh.PrivateKey // ephemeral P-256 client key
	pubKeyC   []byte           // 33-byte compressed public key (CBOR key 0)
	qrSecret  []byte           // 16-byte secret (CBOR key 1)
	createdAt time.Time
	// Payload is the full "FIDO:/<digits>" string to render as a QR code.
	Payload string
}

// qrOperationHint is the CTAP hybrid operation hint. WhatsApp's passkey stage
// is a WebAuthn get (assertion), so "ga".
const qrOperationHint = "ga"

// newCableQR generates a fresh ephemeral client key + QR secret and encodes the
// `FIDO:/` base10 payload.
func newCableQR(knownTunnelDomains int) (*cableQR, error) {
	priv, err := ecdh.P256().GenerateKey(rand.Reader)
	if err != nil {
		return nil, fmt.Errorf("generate caBLE client key: %w", err)
	}
	secret := make([]byte, 16)
	if _, err := rand.Read(secret); err != nil {
		return nil, fmt.Errorf("generate QR secret: %w", err)
	}

	compressed := compressP256(priv.PublicKey().Bytes())

	now := time.Now()
	// CTAP 2.2 §11.5.3 QR map. Integer keys, canonical CBOR.
	qrMap := map[int]any{
		0: compressed,        // initiator compressed public key
		1: secret,            // QR secret
		2: knownTunnelDomains, // number of assigned tunnel domains known
		3: now.Unix(),        // current time (epoch seconds)
		4: false,             // state-assisted transactions unsupported
		5: qrOperationHint,   // operation hint: get-assertion
	}
	encMode, err := cbor.CanonicalEncOptions().EncMode()
	if err != nil {
		return nil, fmt.Errorf("cbor enc mode: %w", err)
	}
	raw, err := encMode.Marshal(qrMap)
	if err != nil {
		return nil, fmt.Errorf("marshal QR CBOR: %w", err)
	}

	return &cableQR{
		privKey:   priv,
		pubKeyC:   compressed,
		qrSecret:  secret,
		createdAt: now,
		Payload:   "FIDO:/" + digitEncode(raw),
	}, nil
}

// compressP256 converts a 65-byte X9.62 uncompressed point (0x04||X||Y) into a
// 33-byte compressed point (0x02/0x03||X).
func compressP256(uncompressed []byte) []byte {
	if len(uncompressed) != 65 || uncompressed[0] != 0x04 {
		// crypto/ecdh always returns 65-byte uncompressed points; guard anyway.
		return uncompressed
	}
	x := uncompressed[1:33]
	y := uncompressed[33:65]
	out := make([]byte, 33)
	// prefix 0x02 if Y is even, 0x03 if odd.
	out[0] = 0x02 | (y[len(y)-1] & 1)
	copy(out[1:], x)
	return out
}

// digitEncode implements the CTAP 2.2 hybrid base10 encoding: the CBOR bytes are
// split into 7-byte chunks, each encoded as a fixed number of zero-padded
// decimal digits (17 per full 7-byte chunk); a partial final chunk uses the
// digit-count lookup table.
func digitEncode(data []byte) string {
	const chunkSize = 7
	const chunkDigits = 17
	// partialDigits[n] = number of decimal digits for a final chunk of n bytes.
	partialDigits := [chunkSize]int{0, 3, 5, 8, 10, 13, 15}

	var out []byte
	for len(data) >= chunkSize {
		chunk := new(big.Int).SetBytes(reverseBytes(data[:chunkSize]))
		out = append(out, padDigits(chunk, chunkDigits)...)
		data = data[chunkSize:]
	}
	if len(data) > 0 {
		chunk := new(big.Int).SetBytes(reverseBytes(data))
		out = append(out, padDigits(chunk, partialDigits[len(data)])...)
	}
	return string(out)
}

// digitDecode reverses digitEncode (used by tests and for symmetry).
func digitDecode(s string) ([]byte, error) {
	const chunkDigits = 17
	const chunkSize = 7
	partialBytes := map[int]int{3: 1, 5: 2, 8: 3, 10: 4, 13: 5, 15: 6}

	var out []byte
	for len(s) >= chunkDigits {
		v, ok := new(big.Int).SetString(s[:chunkDigits], 10)
		if !ok {
			return nil, fmt.Errorf("invalid digit chunk %q", s[:chunkDigits])
		}
		out = append(out, leBytes(v, chunkSize)...)
		s = s[chunkDigits:]
	}
	if len(s) > 0 {
		n, ok := partialBytes[len(s)]
		if !ok {
			return nil, fmt.Errorf("invalid final digit-chunk length %d", len(s))
		}
		v, ok := new(big.Int).SetString(s, 10)
		if !ok {
			return nil, fmt.Errorf("invalid final digit chunk %q", s)
		}
		out = append(out, leBytes(v, n)...)
	}
	return out, nil
}

func padDigits(v *big.Int, width int) []byte {
	s := v.String()
	if len(s) > width {
		// Should not happen for valid 7-byte chunks; truncation would corrupt.
		return []byte(s)
	}
	out := make([]byte, width)
	pad := width - len(s)
	for i := 0; i < pad; i++ {
		out[i] = '0'
	}
	copy(out[pad:], s)
	return out
}

// leBytes returns the little-endian byte representation of v in exactly n bytes.
func leBytes(v *big.Int, n int) []byte {
	be := v.Bytes()
	le := reverseBytes(be)
	out := make([]byte, n)
	copy(out, le)
	return out
}

func reverseBytes(b []byte) []byte {
	out := make([]byte, len(b))
	for i := range b {
		out[len(b)-1-i] = b[i]
	}
	return out
}
