package main

// Software WebAuthn authenticator for WhatsApp's passkey-protected device
// linking flow.
//
// Background: as of 2026-06-30 WhatsApp's servers drive an additional
// passkey/WebAuthn stage between QR acceptance and companion registration (see
// whatsmeow PR #1186 / issue #1184). whatsmeow parses the server's
// passkey_request_options and emits events.PairPasskeyRequest, but it
// deliberately leaves generating the WebAuthn assertion to the integrator
// ("This should be called after receiving an *events.PairPasskeyRequest and
// asking the authenticator for a response"). A browser/OS would use a real
// platform authenticator here; a headless TUI has none, so we embed a minimal
// software authenticator.
//
// The credential is generated once and persisted next to the WhatsApp store so
// re-links reuse the same passkey. The assertion format (ES256 over
// authenticatorData || SHA256(clientDataJSON)) follows the WebAuthn spec; the
// remaining server-specific unknowns (origin string, whether the server
// accepts a self-registered credential) are intentionally easy to tweak and are
// validated against the live handshake via CHATCLI_WHATSAPP_LOG=debug.

import (
	"context"
	"crypto/ecdsa"
	"crypto/elliptic"
	"crypto/rand"
	"crypto/sha256"
	"encoding/base64"
	"encoding/binary"
	"encoding/json"
	"fmt"
	"math/big"
	"os"

	"go.mau.fi/whatsmeow/types"
	"go.mau.fi/whatsmeow/types/events"
)

// passkeyCredential is the persisted software-authenticator credential. The
// private key is stored as the raw P-256 scalar; the public coordinates are
// kept for completeness/attestation.
type passkeyCredential struct {
	CredentialID []byte `json:"credential_id"`
	PrivateKeyD  []byte `json:"private_key_d"`
	PublicKeyX   []byte `json:"public_key_x"`
	PublicKeyY   []byte `json:"public_key_y"`
	SignCount    uint32 `json:"sign_count"`
}

// passkeyOrigin returns the WebAuthn origin to embed in clientDataJSON. WhatsApp
// Web's origin is https://web.whatsapp.com; expose an override so the live test
// can iterate without a rebuild if the server rejects it.
func passkeyOrigin() string {
	if v := os.Getenv("CHATCLI_WHATSAPP_PASSKEY_ORIGIN"); v != "" {
		return v
	}
	return "https://web.whatsapp.com"
}

func (c *client) passkeyPath() string {
	return c.dbPath + ".passkey.json"
}

// handlePasskeyRequest answers a server passkey challenge by producing a
// software WebAuthn assertion and sending it back through whatsmeow.
func (c *client) handlePasskeyRequest(ctx context.Context, req *events.PairPasskeyRequest) {
	if req == nil || req.PublicKey == nil {
		emit(bridgeEvent{Type: "error", Message: "passkey request missing public key"})
		return
	}
	// Instrumentation: dump exactly what the server is demanding. The single
	// most important field is allowCredentials: if it is non-empty, WebAuthn
	// requires the assertion to come from one of the listed (server-registered)
	// credentials, which a self-minted software credential can never satisfy.
	// An empty list means the server accepts any discoverable credential it has
	// on file for the account. Either way the server verifies the signature
	// against a public key it stored at registration time, so this log tells us
	// whether headless linking is even possible without the phone's passkey.
	c.logPasskeyRequestOptions(req.PublicKey)
	emit(bridgeEvent{Type: "login", Event: "passkey-authenticating"})

	// Default path: drive the real caBLE / WebAuthn hybrid ceremony so the
	// phone's registered passkey signs the challenge. The legacy self-mint path
	// (which the server accepts at the IQ level but the phone never continues
	// past) is retained behind CHATCLI_WHATSAPP_CABLE_DISABLE for A/B diagnosis.
	var (
		resp *types.WebAuthnResponse
		err  error
	)
	if loadCableConfig().Disable {
		c.log("passkey: caBLE disabled, using legacy self-mint assertion")
		resp, err = c.buildWebAuthnAssertion(req.PublicKey)
	} else {
		resp, err = c.runCableCeremony(ctx, req.PublicKey)
	}
	if err != nil {
		emit(bridgeEvent{Type: "error", Message: fmt.Sprintf("build passkey assertion: %v", err)})
		return
	}
	c.log("sending passkey assertion credential_id=%s", resp.ID)
	if err := c.wa.SendPasskeyResponse(ctx, resp); err != nil {
		// A server-side rejection of the assertion (e.g. unknown/invalid
		// credential) surfaces here as an IQ error. Logging the raw error is
		// what distinguishes "assertion refused" from "assertion accepted, a
		// later stage failed".
		c.log("passkey assertion REJECTED by server: %v", err)
		emit(bridgeEvent{Type: "error", Message: fmt.Sprintf("send passkey response: %v", err)})
		return
	}
	// The assertion was accepted (server returned an IQ result). Pairing is NOT
	// done yet: the server must still deliver the primary_ephemeral_identity
	// continuation from the phone, which whatsmeow turns into a confirmation
	// (auto-confirmed when a valid pairing_handoff_proof was sent). Surface this
	// distinct state so a stall here is not misreported as a generic QR timeout
	// and so the user knows to finish the prompt on their phone promptly — the
	// QR emitter will disconnect the client once its code budget runs out.
	c.log("passkey assertion ACCEPTED by server (awaiting phone continuation / crsc_continuation)")
	emit(bridgeEvent{Type: "login", Event: "passkey-awaiting-phone"})
}

// logPasskeyRequestOptions records the server's WebAuthn request options so a
// single instrumented link attempt reveals whether the challenge is pinned to a
// specific (phone-registered) credential.
func (c *client) logPasskeyRequestOptions(pub *types.WebAuthnPublicKey) {
	c.log(
		"passkey request options rpId=%q userVerification=%q timeout=%d challenge_len=%d allow_credentials=%d extensions=%d",
		pub.RelyingPartID,
		pub.UserVerification,
		pub.Timeout,
		len(pub.Challenge),
		len(pub.AllowCredentials),
		len(pub.Extensions),
	)
	for i, cred := range pub.AllowCredentials {
		c.log(
			"passkey allowCredentials[%d] type=%q id=%s transports=%v",
			i,
			cred.Type,
			base64.RawURLEncoding.EncodeToString(cred.ID),
			cred.Transports,
		)
	}
	if len(pub.AllowCredentials) == 0 {
		c.log("passkey allowCredentials is EMPTY (server allows any discoverable credential it has registered)")
	}
}

// handlePasskeyConfirmation is invoked when the server requires the user to
// verify a pairing code (SkipHandoffUX == false; the auto-confirm path is
// handled inside whatsmeow's QR channel). Stage A surfaces the code and
// confirms immediately; Stage B gates confirmation on explicit user approval.
func (c *client) handlePasskeyConfirmation(ctx context.Context, conf *events.PairPasskeyConfirmation) {
	if conf == nil {
		return
	}
	// The verification code is surfaced for display by the global event handler
	// (handleWhatsAppEvent's *events.PairPasskeyConfirmation case), which fires
	// on both the auto-confirm and manual paths. Here we only perform the
	// confirmation IQ for the manual (SkipHandoffUX == false) path routed via
	// the QR channel; emitting the code again would duplicate the login event.
	if err := c.wa.SendPasskeyConfirmation(ctx); err != nil {
		emit(bridgeEvent{Type: "error", Message: fmt.Sprintf("send passkey confirmation: %v", err)})
	}
}

// loadOrCreatePasskey returns the persisted credential, generating and saving a
// fresh P-256 credential on first use.
func (c *client) loadOrCreatePasskey() (*passkeyCredential, *ecdsa.PrivateKey, error) {
	path := c.passkeyPath()
	if data, err := os.ReadFile(path); err == nil {
		var cred passkeyCredential
		if err := json.Unmarshal(data, &cred); err != nil {
			return nil, nil, fmt.Errorf("parse stored passkey: %w", err)
		}
		priv := &ecdsa.PrivateKey{D: new(big.Int).SetBytes(cred.PrivateKeyD)}
		priv.PublicKey.Curve = elliptic.P256()
		priv.PublicKey.X = new(big.Int).SetBytes(cred.PublicKeyX)
		priv.PublicKey.Y = new(big.Int).SetBytes(cred.PublicKeyY)
		return &cred, priv, nil
	} else if !os.IsNotExist(err) {
		return nil, nil, fmt.Errorf("read stored passkey: %w", err)
	}

	priv, err := ecdsa.GenerateKey(elliptic.P256(), rand.Reader)
	if err != nil {
		return nil, nil, fmt.Errorf("generate passkey: %w", err)
	}
	credID := make([]byte, 32)
	if _, err := rand.Read(credID); err != nil {
		return nil, nil, fmt.Errorf("generate credential id: %w", err)
	}
	cred := &passkeyCredential{
		CredentialID: credID,
		PrivateKeyD:  priv.D.Bytes(),
		PublicKeyX:   priv.PublicKey.X.Bytes(),
		PublicKeyY:   priv.PublicKey.Y.Bytes(),
		SignCount:    0,
	}
	if err := c.savePasskey(cred); err != nil {
		return nil, nil, err
	}
	return cred, priv, nil
}

func (c *client) savePasskey(cred *passkeyCredential) error {
	data, err := json.Marshal(cred)
	if err != nil {
		return fmt.Errorf("marshal passkey: %w", err)
	}
	if err := os.WriteFile(c.passkeyPath(), data, 0o600); err != nil {
		return fmt.Errorf("write passkey: %w", err)
	}
	return nil
}

// buildWebAuthnAssertion produces a WebAuthn "get" assertion for the given
// server-provided public-key request options.
func (c *client) buildWebAuthnAssertion(pub *types.WebAuthnPublicKey) (*types.WebAuthnResponse, error) {
	cred, priv, err := c.loadOrCreatePasskey()
	if err != nil {
		return nil, err
	}

	rpID := pub.RelyingPartID
	if rpID == "" {
		rpID = "whatsapp.com"
	}

	// clientDataJSON: the challenge is echoed as unpadded base64url per spec.
	clientData := map[string]any{
		"type":        "webauthn.get",
		"challenge":   base64.RawURLEncoding.EncodeToString(pub.Challenge),
		"origin":      passkeyOrigin(),
		"crossOrigin": false,
	}
	clientDataJSON, err := json.Marshal(clientData)
	if err != nil {
		return nil, fmt.Errorf("marshal clientDataJSON: %w", err)
	}

	// authenticatorData = SHA256(rpID) || flags || signCount.
	// flags: UP (0x01) | UV (0x04) — user present and verified.
	rpIDHash := sha256.Sum256([]byte(rpID))
	cred.SignCount++
	authData := make([]byte, 0, 37)
	authData = append(authData, rpIDHash[:]...)
	authData = append(authData, 0x05)
	var counter [4]byte
	binary.BigEndian.PutUint32(counter[:], cred.SignCount)
	authData = append(authData, counter[:]...)

	// signature = ECDSA-P256-SHA256 over authenticatorData || SHA256(clientDataJSON),
	// ASN.1 DER encoded as required by WebAuthn.
	clientDataHash := sha256.Sum256(clientDataJSON)
	signed := append(append([]byte{}, authData...), clientDataHash[:]...)
	digest := sha256.Sum256(signed)
	sig, err := ecdsa.SignASN1(rand.Reader, priv, digest[:])
	if err != nil {
		return nil, fmt.Errorf("sign assertion: %w", err)
	}

	if err := c.savePasskey(cred); err != nil {
		return nil, err
	}

	return &types.WebAuthnResponse{
		ID:    base64.RawURLEncoding.EncodeToString(cred.CredentialID),
		RawID: cred.CredentialID,
		Type:  "public-key",
		Response: types.WebAuthnResponseData{
			ClientDataJSON:    clientDataJSON,
			AuthenticatorData: authData,
			Signature:         sig,
			UserHandle:        nil,
		},
	}, nil
}
