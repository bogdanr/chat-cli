package main

import (
	"bytes"
	"crypto/ecdh"
	"crypto/rand"
	"testing"
)

// TestNoiseKNpsk0RoundTrip verifies the initiator produces a channel that a
// spec-faithful responder can complete, and that transport ciphers agree in
// both directions. This proves internal consistency of the Noise code; live
// interop with the phone still requires a real handshake (see plan Risk 4).
func TestNoiseKNpsk0RoundTrip(t *testing.T) {
	staticKey, err := ecdh.P256().GenerateKey(rand.Reader)
	if err != nil {
		t.Fatalf("static key: %v", err)
	}
	psk := bytes.Repeat([]byte{0x5a}, pskSize)

	ni := newNoiseInitiator(staticKey, psk)
	msg1, err := ni.writeMessage1()
	if err != nil {
		t.Fatalf("writeMessage1: %v", err)
	}

	// ---- Responder (test-only, mirrors KNpsk0) ----
	rss := newSymmetricState()
	rss.mixHash([]byte{0x01})                  // caBLE KNpsk0 prologue byte
	rss.mixHash(staticKey.PublicKey().Bytes()) // pre-message "-> s"

	// message1: psk, e
	rss.mixKeyAndHash(psk)
	const pubLen = 65
	initEphPubBytes := msg1[:pubLen]
	rss.mixHash(initEphPubBytes)
	rss.mixKey(initEphPubBytes)
	if _, err := rss.decryptAndHash(msg1[pubLen:]); err != nil {
		t.Fatalf("responder decrypt msg1 payload: %v", err)
	}

	initEphPub, err := ecdh.P256().NewPublicKey(initEphPubBytes)
	if err != nil {
		t.Fatalf("init eph pub: %v", err)
	}

	// message2: e, ee, se
	respE, err := ecdh.P256().GenerateKey(rand.Reader)
	if err != nil {
		t.Fatalf("resp eph: %v", err)
	}
	respEPub := respE.PublicKey().Bytes()
	rss.mixHash(respEPub)
	rss.mixKey(respEPub)

	ee, err := respE.ECDH(initEphPub)
	if err != nil {
		t.Fatalf("resp ee: %v", err)
	}
	rss.mixKey(ee)

	se, err := respE.ECDH(staticKey.PublicKey())
	if err != nil {
		t.Fatalf("resp se: %v", err)
	}
	rss.mixKey(se)

	payload2, err := rss.encryptAndHash(nil)
	if err != nil {
		t.Fatalf("responder encrypt payload2: %v", err)
	}
	msg2 := append(append([]byte{}, respEPub...), payload2...)

	// initiator processes message2
	if _, err := ni.readMessage2(msg2); err != nil {
		t.Fatalf("readMessage2: %v", err)
	}
	if !ni.complete {
		t.Fatalf("initiator handshake not complete")
	}

	respSend, respRecv := rss.split() // out[0]=init->resp, out[1]=resp->init

	// Directional transport test.
	plaintext := []byte("hello-ctap")
	ct, err := ni.sendCS.encrypt(nil, plaintext)
	if err != nil {
		t.Fatalf("initiator encrypt: %v", err)
	}
	got, err := respSend.decrypt(nil, ct) // responder's recv side is out[0]
	if err != nil {
		t.Fatalf("responder decrypt: %v", err)
	}
	if !bytes.Equal(got, plaintext) {
		t.Fatalf("init->resp mismatch: %q", got)
	}

	reply := []byte("assertion-bytes")
	ct2, err := respRecv.encrypt(nil, reply) // responder's send side is out[1]
	if err != nil {
		t.Fatalf("responder encrypt reply: %v", err)
	}
	got2, err := ni.recvCS.decrypt(nil, ct2)
	if err != nil {
		t.Fatalf("initiator decrypt reply: %v", err)
	}
	if !bytes.Equal(got2, reply) {
		t.Fatalf("resp->init mismatch: %q", got2)
	}
}

// TestTransportPadding verifies caBLE transport padding rounds up to a 32-byte
// multiple with a trailing count byte, and round-trips (Chromium Crypter).
func TestTransportPadding(t *testing.T) {
	for _, in := range [][]byte{{}, []byte("x"), bytes.Repeat([]byte{7}, 31), bytes.Repeat([]byte{7}, 32), bytes.Repeat([]byte{7}, 40)} {
		padded := padTransport(in)
		if len(padded)%transportPadGranularity != 0 {
			t.Fatalf("padded len %d not a multiple of %d", len(padded), transportPadGranularity)
		}
		if len(padded) <= len(in) {
			t.Fatalf("padded len %d must exceed input %d", len(padded), len(in))
		}
		got, err := unpadTransport(padded)
		if err != nil {
			t.Fatalf("unpadTransport: %v", err)
		}
		if !bytes.Equal(got, in) {
			t.Fatalf("round-trip mismatch: got %x want %x", got, in)
		}
	}
}

// TestNonceLayouts pins the two distinct caBLE nonce layouts.
func TestNonceLayouts(t *testing.T) {
	h := handshakeNonce(1)
	if h[0] != 0 || h[1] != 0 || h[2] != 0 || h[3] != 1 {
		t.Fatalf("handshakeNonce(1) = %x, want 4-byte BE counter at front", h)
	}
	tr := transportNonce(1)
	if tr[8] != 0 || tr[9] != 0 || tr[10] != 0 || tr[11] != 1 {
		t.Fatalf("transportNonce(1) = %x, want 4-byte BE counter at offset 8", tr)
	}
	for i := 0; i < 8; i++ {
		if tr[i] != 0 {
			t.Fatalf("transportNonce(1) byte %d not zero: %x", i, tr)
		}
	}
}
