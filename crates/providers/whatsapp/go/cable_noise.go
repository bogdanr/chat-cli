package main

// Noise KNpsk0 handshake for the caBLE tunnel (Milestone 4).
//
// caBLE runs a Noise handshake over the tunnel WebSocket before exchanging
// CTAP2 messages. For the QR flow the pattern is KNpsk0:
//
//	-> s              (pre-message: the client's QR public key, known to phone)
//	...
//	-> psk, e
//	<- e, ee, se
//
// with P-256 for DH, AES-256-GCM for the cipher, and SHA-256 for hashing. The
// PSK is derived from the QR secret and BLE nonce.
//
// This follows the standard Noise Protocol Framework (rev 34), which caBLE
// adheres to. The GCM nonce format and protocol name are the two spots most
// likely to need adjustment against a live transcript (plan Risk 4); both are
// isolated here.

import (
	"crypto/aes"
	"crypto/cipher"
	"crypto/ecdh"
	"crypto/sha256"
	"encoding/binary"
	"fmt"

	"golang.org/x/crypto/hkdf"
)

// noiseProtocolName is the caBLE Noise protocol identifier.
const noiseProtocolName = "Noise_KNpsk0_P256_AESGCM_SHA256"

type symmetricState struct {
	ck   [32]byte
	h    [32]byte
	k    [32]byte
	hasK bool
	n    uint64
}

func newSymmetricState() *symmetricState {
	s := &symmetricState{}
	name := []byte(noiseProtocolName)
	if len(name) <= 32 {
		copy(s.h[:], name)
	} else {
		s.h = sha256.Sum256(name)
	}
	s.ck = s.h
	return s
}

func hkdfN(chainingKey, input []byte, n int) [][32]byte {
	r := hkdf.New(sha256.New, input, chainingKey, nil)
	out := make([][32]byte, n)
	for i := 0; i < n; i++ {
		if _, err := r.Read(out[i][:]); err != nil {
			panic(fmt.Sprintf("noise hkdf: %v", err))
		}
	}
	return out
}

func (s *symmetricState) mixKey(input []byte) {
	out := hkdfN(s.ck[:], input, 2)
	s.ck = out[0]
	s.k = out[1]
	s.hasK = true
	s.n = 0
}

func (s *symmetricState) mixHash(data []byte) {
	hh := sha256.New()
	hh.Write(s.h[:])
	hh.Write(data)
	copy(s.h[:], hh.Sum(nil))
}

func (s *symmetricState) mixKeyAndHash(psk []byte) {
	out := hkdfN(s.ck[:], psk, 3)
	s.ck = out[0]
	s.mixHash(out[1][:])
	s.k = out[2]
	s.hasK = true
	s.n = 0
}

// handshakeNonce builds the 96-bit nonce used by the Noise symmetric state
// during the handshake: a 4-byte big-endian counter at the FRONT, the trailing
// 8 bytes zero (Chromium Noise::EncryptAndHash, noise.cc:100-103). The counter
// resets to 0 on every MixKey/MixKeyAndHash, so in practice it is always 0 here.
func handshakeNonce(n uint64) []byte {
	var nonce [12]byte
	binary.BigEndian.PutUint32(nonce[0:4], uint32(n))
	return nonce[:]
}

// transportNonce builds the 96-bit nonce used by the post-handshake transport
// Crypter: 8 zero bytes followed by a 4-byte big-endian counter (Chromium
// Crypter::ConstructNonce, v2_handshake.cc:62-70). This deliberately differs
// from handshakeNonce; the two layers use different layouts.
func transportNonce(n uint64) []byte {
	var nonce [12]byte
	binary.BigEndian.PutUint32(nonce[8:12], uint32(n))
	return nonce[:]
}

// transportPadGranularity is the block size caBLE pads transport messages to
// (Chromium Crypter::Encrypt, v2_handshake.cc:795).
const transportPadGranularity = 32

// padTransport appends zero bytes plus a trailing count byte so the total
// length is a multiple of transportPadGranularity (Chromium Crypter::Encrypt).
func padTransport(msg []byte) []byte {
	padded := (len(msg) + 1 + transportPadGranularity - 1) &^ (transportPadGranularity - 1)
	out := make([]byte, padded)
	copy(out, msg)
	out[padded-1] = byte(padded - len(msg) - 1)
	return out
}

// unpadTransport strips the trailing count byte and its zero padding (Chromium
// Crypter::Decrypt).
func unpadTransport(plaintext []byte) ([]byte, error) {
	if len(plaintext) == 0 {
		return nil, fmt.Errorf("empty transport message")
	}
	padLen := int(plaintext[len(plaintext)-1])
	if padLen+1 > len(plaintext) {
		return nil, fmt.Errorf("invalid transport padding %d", padLen)
	}
	return plaintext[:len(plaintext)-padLen-1], nil
}

func (s *symmetricState) aead() (cipher.AEAD, error) {
	block, err := aes.NewCipher(s.k[:])
	if err != nil {
		return nil, err
	}
	return cipher.NewGCM(block)
}

func (s *symmetricState) encryptAndHash(plaintext []byte) ([]byte, error) {
	if !s.hasK {
		s.mixHash(plaintext)
		return plaintext, nil
	}
	aead, err := s.aead()
	if err != nil {
		return nil, err
	}
	ct := aead.Seal(nil, handshakeNonce(s.n), plaintext, s.h[:])
	s.n++
	s.mixHash(ct)
	return ct, nil
}

func (s *symmetricState) decryptAndHash(ciphertext []byte) ([]byte, error) {
	if !s.hasK {
		s.mixHash(ciphertext)
		return ciphertext, nil
	}
	aead, err := s.aead()
	if err != nil {
		return nil, err
	}
	pt, err := aead.Open(nil, handshakeNonce(s.n), ciphertext, s.h[:])
	if err != nil {
		return nil, fmt.Errorf("noise decrypt: %w", err)
	}
	s.n++
	s.mixHash(ciphertext)
	return pt, nil
}

// cipherState is a post-handshake transport cipher. caBLE transport frames are
// zero-padded to a 32-byte multiple and use empty AAD (Chromium Crypter).
type cipherState struct {
	k [32]byte
	n uint64
}

func (c *cipherState) encrypt(ad, plaintext []byte) ([]byte, error) {
	block, err := aes.NewCipher(c.k[:])
	if err != nil {
		return nil, err
	}
	aead, err := cipher.NewGCM(block)
	if err != nil {
		return nil, err
	}
	ct := aead.Seal(nil, transportNonce(c.n), padTransport(plaintext), ad)
	c.n++
	return ct, nil
}

func (c *cipherState) decrypt(ad, ciphertext []byte) ([]byte, error) {
	block, err := aes.NewCipher(c.k[:])
	if err != nil {
		return nil, err
	}
	aead, err := cipher.NewGCM(block)
	if err != nil {
		return nil, err
	}
	pt, err := aead.Open(nil, transportNonce(c.n), ciphertext, ad)
	if err != nil {
		return nil, err
	}
	c.n++
	return unpadTransport(pt)
}

func (s *symmetricState) split() (*cipherState, *cipherState) {
	out := hkdfN(s.ck[:], nil, 2)
	return &cipherState{k: out[0]}, &cipherState{k: out[1]}
}

// noiseInitiator drives the KNpsk0 handshake from the client side.
type noiseInitiator struct {
	ss       *symmetricState
	staticK  *ecdh.PrivateKey // client QR key ("s")
	localE   *ecdh.PrivateKey // client ephemeral ("e")
	psk      []byte
	sendCS   *cipherState
	recvCS   *cipherState
	complete bool
}

func newNoiseInitiator(staticKey *ecdh.PrivateKey, psk []byte) *noiseInitiator {
	ss := newSymmetricState()
	// caBLE prepends a one-byte prologue before the pre-message. For the QR
	// (KNpsk0) flow the prologue byte is 0x01 (Chromium HandshakeInitiator::
	// BuildInitialMessage, v2_handshake.cc:908-909). This MUST be mixed before
	// the static key, or the handshake hash diverges from the phone's and it
	// silently drops the tunnel (observed as EOF reading message2).
	ss.mixHash([]byte{0x01})
	// Pre-message: "-> s" means the responder knows our static public key (from
	// the QR). Both parties MixHash the uncompressed static public key.
	ss.mixHash(staticKey.PublicKey().Bytes())
	return &noiseInitiator{ss: ss, staticK: staticKey, psk: psk}
}

// writeMessage1 produces the first handshake message: [psk, e] plus an empty
// encrypted payload.
func (ni *noiseInitiator) writeMessage1() ([]byte, error) {
	// psk0: mix the PSK first.
	ni.ss.mixKeyAndHash(ni.psk)

	e, err := ecdh.P256().GenerateKey(rngReader)
	if err != nil {
		return nil, fmt.Errorf("noise ephemeral: %w", err)
	}
	ni.localE = e
	epub := e.PublicKey().Bytes()
	ni.ss.mixHash(epub)
	ni.ss.mixKey(epub)

	payload, err := ni.ss.encryptAndHash(nil)
	if err != nil {
		return nil, err
	}
	msg := append([]byte{}, epub...)
	msg = append(msg, payload...)
	return msg, nil
}

// readMessage2 processes the responder's reply: [e, ee, se].
func (ni *noiseInitiator) readMessage2(msg []byte) ([]byte, error) {
	const pubLen = 65 // P-256 uncompressed
	if len(msg) < pubLen {
		return nil, fmt.Errorf("noise message2 too short: %d", len(msg))
	}
	rePubBytes := msg[:pubLen]
	rest := msg[pubLen:]

	rePub, err := ecdh.P256().NewPublicKey(rePubBytes)
	if err != nil {
		return nil, fmt.Errorf("noise responder ephemeral: %w", err)
	}
	ni.ss.mixHash(rePubBytes)
	// psk-pattern rule: every "e" token also mixes the ephemeral public key
	// into the chaining key, including the responder's ephemeral.
	ni.ss.mixKey(rePubBytes)

	// ee: DH(local ephemeral, responder ephemeral)
	ee, err := ni.localE.ECDH(rePub)
	if err != nil {
		return nil, fmt.Errorf("noise ee: %w", err)
	}
	ni.ss.mixKey(ee)

	// se: DH(local static, responder ephemeral)
	se, err := ni.staticK.ECDH(rePub)
	if err != nil {
		return nil, fmt.Errorf("noise se: %w", err)
	}
	ni.ss.mixKey(se)

	payload, err := ni.ss.decryptAndHash(rest)
	if err != nil {
		return nil, err
	}
	ni.sendCS, ni.recvCS = ni.ss.split()
	ni.complete = true
	return payload, nil
}
