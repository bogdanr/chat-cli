package main

// CTAP2 authenticatorGetAssertion over the caBLE tunnel (Milestones 5 & 6).
//
// After the Noise handshake, the client sends a CTAP2 getAssertion built from
// the whatsmeow-provided WebAuthn request options; the phone signs with its
// real WhatsApp passkey and returns an assertion, which is mapped into
// types.WebAuthnResponse for cli.SendPasskeyResponse.

import (
	"crypto/sha256"
	"encoding/base64"
	"encoding/json"
	"fmt"

	"github.com/fxamacker/cbor/v2"
	"go.mau.fi/util/jsonbytes"
	"go.mau.fi/whatsmeow/types"
)

// ctapCmdGetAssertion is the CTAP2 command byte for authenticatorGetAssertion.
const ctapCmdGetAssertion = 0x02

// buildClientDataJSON constructs the WebAuthn clientDataJSON for a get
// assertion. The exact bytes are hashed for the CTAP clientDataHash AND stored
// verbatim in the WebAuthnResponse, so they must be produced once and reused.
func buildClientDataJSON(challenge []byte, origin string) ([]byte, error) {
	clientData := map[string]any{
		"type":        "webauthn.get",
		"challenge":   base64.RawURLEncoding.EncodeToString(challenge),
		"origin":      origin,
		"crossOrigin": false,
	}
	return json.Marshal(clientData)
}

// buildGetAssertionCommand encodes the CTAP2 getAssertion request bytes
// (command byte || CBOR map).
func buildGetAssertionCommand(pub *types.WebAuthnPublicKey, clientDataHash []byte) ([]byte, error) {
	rpID := pub.RelyingPartID
	if rpID == "" {
		rpID = "whatsapp.com"
	}

	params := map[int]any{
		1: rpID,          // rpId
		2: clientDataHash, // clientDataHash
	}

	// allowList (key 3): map each allowed credential.
	if len(pub.AllowCredentials) > 0 {
		allow := make([]map[string]any, 0, len(pub.AllowCredentials))
		for _, c := range pub.AllowCredentials {
			allow = append(allow, map[string]any{
				"type": "public-key",
				"id":   []byte(c.ID),
			})
		}
		params[3] = allow
	}

	// options (key 5): user verification.
	if pub.UserVerification == "required" || pub.UserVerification == "preferred" {
		params[5] = map[string]any{"uv": true}
	}

	encMode, err := cbor.CTAP2EncOptions().EncMode()
	if err != nil {
		return nil, fmt.Errorf("ctap2 enc mode: %w", err)
	}
	body, err := encMode.Marshal(params)
	if err != nil {
		return nil, fmt.Errorf("marshal getAssertion: %w", err)
	}
	return append([]byte{ctapCmdGetAssertion}, body...), nil
}

// ctapAssertionResponse holds the parsed CTAP2 getAssertion response.
type ctapAssertionResponse struct {
	CredentialID        []byte
	AuthenticatorData   []byte
	Signature           []byte
	UserHandle          []byte
	NumberOfCredentials int
}

// parseGetAssertionResponse parses the CTAP2 response bytes (status byte ||
// CBOR map).
func parseGetAssertionResponse(data []byte) (*ctapAssertionResponse, error) {
	if len(data) == 0 {
		return nil, fmt.Errorf("empty CTAP response")
	}
	if status := data[0]; status != 0x00 {
		return nil, fmt.Errorf("CTAP getAssertion error status %#x", status)
	}
	var raw map[int]cbor.RawMessage
	if err := cbor.Unmarshal(data[1:], &raw); err != nil {
		return nil, fmt.Errorf("decode getAssertion response: %w", err)
	}

	out := &ctapAssertionResponse{}

	// key 1: credential {type, id}
	if v, ok := raw[1]; ok {
		var cred struct {
			ID []byte `cbor:"id"`
		}
		if err := cbor.Unmarshal(v, &cred); err != nil {
			return nil, fmt.Errorf("decode credential: %w", err)
		}
		out.CredentialID = cred.ID
	}
	// key 2: authData
	if v, ok := raw[2]; ok {
		if err := cbor.Unmarshal(v, &out.AuthenticatorData); err != nil {
			return nil, fmt.Errorf("decode authData: %w", err)
		}
	}
	// key 3: signature
	if v, ok := raw[3]; ok {
		if err := cbor.Unmarshal(v, &out.Signature); err != nil {
			return nil, fmt.Errorf("decode signature: %w", err)
		}
	}
	// key 4: user {id}
	if v, ok := raw[4]; ok {
		var user struct {
			ID []byte `cbor:"id"`
		}
		if err := cbor.Unmarshal(v, &user); err != nil {
			return nil, fmt.Errorf("decode user: %w", err)
		}
		out.UserHandle = user.ID
	}
	// key 5: numberOfCredentials
	if v, ok := raw[5]; ok {
		_ = cbor.Unmarshal(v, &out.NumberOfCredentials)
	}

	if len(out.AuthenticatorData) == 0 || len(out.Signature) == 0 {
		return nil, fmt.Errorf("CTAP response missing authData or signature")
	}
	return out, nil
}

// toWebAuthnResponse maps a parsed CTAP assertion into the whatsmeow type. The
// clientDataJSON passed here must be byte-identical to the one whose SHA-256 was
// sent as the CTAP clientDataHash, or the server's signature check fails.
func (r *ctapAssertionResponse) toWebAuthnResponse(clientDataJSON []byte) *types.WebAuthnResponse {
	var userHandle *jsonbytes.UnpaddedURLBytes
	if len(r.UserHandle) > 0 {
		uh := jsonbytes.UnpaddedURLBytes(r.UserHandle)
		userHandle = &uh
	}
	return &types.WebAuthnResponse{
		ID:    base64.RawURLEncoding.EncodeToString(r.CredentialID),
		RawID: r.CredentialID,
		Type:  "public-key",
		Response: types.WebAuthnResponseData{
			ClientDataJSON:    clientDataJSON,
			AuthenticatorData: r.AuthenticatorData,
			Signature:         r.Signature,
			UserHandle:        userHandle,
		},
	}
}

// sha256Sum is a small helper kept local to avoid re-importing in callers.
func sha256Sum(b []byte) []byte {
	h := sha256.Sum256(b)
	return h[:]
}
