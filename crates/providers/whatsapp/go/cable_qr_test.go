package main

import (
	"bytes"
	"crypto/rand"
	"strings"
	"testing"

	"github.com/fxamacker/cbor/v2"
)

func TestDigitEncodeDecodeRoundTrip(t *testing.T) {
	for _, size := range []int{0, 1, 2, 3, 6, 7, 8, 14, 15, 20, 33, 64} {
		buf := make([]byte, size)
		if _, err := rand.Read(buf); err != nil {
			t.Fatalf("rand: %v", err)
		}
		enc := digitEncode(buf)
		for _, r := range enc {
			if r < '0' || r > '9' {
				t.Fatalf("non-digit %q in encoding of size %d", r, size)
			}
		}
		dec, err := digitDecode(enc)
		if err != nil {
			t.Fatalf("digitDecode(size=%d): %v", size, err)
		}
		if !bytes.Equal(dec, buf) {
			t.Fatalf("round-trip mismatch size=%d: got %x want %x", size, dec, buf)
		}
	}
}

func TestNewCableQRStructure(t *testing.T) {
	qr, err := newCableQR(len(assignedTunnelDomains))
	if err != nil {
		t.Fatalf("newCableQR: %v", err)
	}
	if !strings.HasPrefix(qr.Payload, "FIDO:/") {
		t.Fatalf("payload missing FIDO:/ prefix: %q", qr.Payload)
	}
	if len(qr.qrSecret) != 16 {
		t.Fatalf("qr secret len %d, want 16", len(qr.qrSecret))
	}
	if len(qr.pubKeyC) != 33 || (qr.pubKeyC[0] != 0x02 && qr.pubKeyC[0] != 0x03) {
		t.Fatalf("bad compressed pubkey: %x", qr.pubKeyC)
	}

	raw, err := digitDecode(strings.TrimPrefix(qr.Payload, "FIDO:/"))
	if err != nil {
		t.Fatalf("decode payload: %v", err)
	}
	var m map[int]cbor.RawMessage
	if err := cbor.Unmarshal(raw, &m); err != nil {
		t.Fatalf("cbor decode: %v", err)
	}
	for _, key := range []int{0, 1, 2, 3, 4, 5} {
		if _, ok := m[key]; !ok {
			t.Fatalf("QR CBOR missing key %d", key)
		}
	}
	var hint string
	if err := cbor.Unmarshal(m[5], &hint); err != nil || hint != qrOperationHint {
		t.Fatalf("operation hint = %q (err %v), want %q", hint, err, qrOperationHint)
	}
}

func TestCompressP256(t *testing.T) {
	// Uncompressed with even Y → 0x02 prefix.
	unc := make([]byte, 65)
	unc[0] = 0x04
	unc[64] = 0x02 // even last byte of Y
	c := compressP256(unc)
	if c[0] != 0x02 {
		t.Fatalf("even Y prefix = %#x, want 0x02", c[0])
	}
	unc[64] = 0x03 // odd
	c = compressP256(unc)
	if c[0] != 0x03 {
		t.Fatalf("odd Y prefix = %#x, want 0x03", c[0])
	}
}
