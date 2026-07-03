package main

// caBLE ceremony orchestration (Milestones 6 & 7).
//
// Ties the layers together into one cancellable ceremony:
//
//	QR → BLE advert → tunnel WebSocket → Noise KNpsk0 → CTAP2 getAssertion
//	→ types.WebAuthnResponse
//
// Environment overrides (all optional), mirroring passkey.go's
// CHATCLI_WHATSAPP_PASSKEY_ORIGIN pattern:
//
//	CHATCLI_WHATSAPP_CABLE_DISABLE        fall back to the legacy self-mint path
//	CHATCLI_WHATSAPP_CABLE_TUNNEL_DOMAIN  force the tunnel server hostname
//	CHATCLI_WHATSAPP_CABLE_BLE_ADAPTER    hciN adapter (default hci0)
//	CHATCLI_WHATSAPP_CABLE_TIMEOUT        ceremony timeout in seconds
//	CHATCLI_WHATSAPP_CABLE_DUMP           hex-dump frames to the debug log
//	CHATCLI_WHATSAPP_PASSKEY_ORIGIN       WebAuthn origin in clientDataJSON
//
// Platform note: the BLE stage is Linux/BlueZ only (cable_ble_other.go returns
// an explicit unsupported error elsewhere).

import (
	"context"
	"fmt"

	"go.mau.fi/whatsmeow/types"
)

// caBLE post-handshake message type byte. Transport frames carry a leading
// MessageType byte (Chromium v2_constants.h: kCTAP=1); the phone rejects the
// frame if this is wrong.
const cableMsgTypeCTAP = 0x01

// runCableCeremony performs the full hybrid ceremony and returns a WebAuthn
// assertion signed by the phone's real passkey.
func (c *client) runCableCeremony(ctx context.Context, pub *types.WebAuthnPublicKey) (*types.WebAuthnResponse, error) {
	cfg := loadCableConfig()
	ctx, cancel := context.WithTimeout(ctx, cfg.Timeout)
	defer cancel()

	// 1. Generate the caBLE QR and surface it to the UI.
	qr, err := newCableQR(len(assignedTunnelDomains))
	if err != nil {
		return nil, err
	}
	c.log("cable: emitting caBLE QR (len=%d)", len(qr.Payload))
	emit(bridgeEvent{Type: "login", Event: "passkey-cable-qr", Code: qr.Payload})

	// 2. Derive the EID key and scan BLE for the phone's advert.
	eidKey, err := cableDerive(qr.qrSecret, nil, deriveEIDKey, eidKeySize)
	if err != nil {
		return nil, err
	}
	emit(bridgeEvent{Type: "login", Event: "passkey-cable-ble-waiting"})
	comp, advert, err := scanCableAdvert(ctx, cfg, eidKey, c.log)
	if err != nil {
		emit(bridgeEvent{Type: "error", Message: fmt.Sprintf("caBLE BLE scan failed: %v", err)})
		return nil, err
	}
	if cfg.Dump {
		c.log("cable: advert=%x routing=%x domain=%d nonce=%x", advert, comp.RoutingID, comp.TunnelDomain, comp.Nonce)
	}

	// 3. Resolve the tunnel domain and open the WebSocket.
	domain := cfg.TunnelDomain
	if domain == "" {
		domain, err = decodeTunnelDomain(comp.TunnelDomain)
		if err != nil {
			emit(bridgeEvent{Type: "error", Message: err.Error()})
			return nil, err
		}
	}
	// Tunnel ID uses an EMPTY salt (Chromium: Derive(secret, {}, kTunnelID)).
	// The phone created the tunnel at /cable/new/<tunnel_id> with this same
	// derivation; a mismatched id means the server has no tunnel to join and
	// rejects the WebSocket upgrade with HTTP 418.
	tunnelID, err := cableDerive(qr.qrSecret, nil, deriveTunnelID, tunnelIDSize)
	if err != nil {
		return nil, err
	}
	conn, err := dialTunnel(ctx, domain, comp.RoutingID, tunnelID)
	if err != nil {
		emit(bridgeEvent{Type: "error", Message: fmt.Sprintf("caBLE tunnel dial failed: %v", err)})
		return nil, err
	}
	defer conn.close()
	emit(bridgeEvent{Type: "login", Event: "passkey-cable-tunnel-connected"})

	// 4. Noise KNpsk0 handshake.
	// PSK salt is the FULL decrypted 16-byte EID, not just the nonce
	// (Chromium: Derive(secret, decrypted_eid, kPSK)).
	psk, err := cableDerive(qr.qrSecret, comp.plaintextEID(), derivePSK, pskSize)
	if err != nil {
		return nil, err
	}
	ni := newNoiseInitiator(qr.privKey, psk)
	msg1, err := ni.writeMessage1()
	if err != nil {
		return nil, err
	}
	if err := conn.write(ctx, msg1); err != nil {
		return nil, fmt.Errorf("send noise message1: %w", err)
	}
	msg2, err := conn.read(ctx)
	if err != nil {
		return nil, fmt.Errorf("read noise message2: %w", err)
	}
	handshakePayload, err := ni.readMessage2(msg2)
	if err != nil {
		emit(bridgeEvent{Type: "error", Message: fmt.Sprintf("caBLE noise handshake failed: %v", err)})
		return nil, err
	}
	// Per Chromium the handshake message2 payload is empty; the getInfo arrives
	// as a SEPARATE post-handshake transport frame (below), not folded in here.
	if len(handshakePayload) != 0 && cfg.Dump {
		c.log("cable: unexpected non-empty handshake payload (%d bytes): %x", len(handshakePayload), handshakePayload)
	}
	c.log("cable: noise handshake complete")

	// 4b. Post-handshake message: the phone sends its getInfo response (a padded
	// CBOR map) as the first transport frame before accepting any command
	// (Chromium kWaitingForPostHandshakeMessage). We must consume it so the
	// transport read sequence stays aligned; its contents are informational.
	postHS, err := conn.read(ctx)
	if err != nil {
		return nil, fmt.Errorf("read post-handshake message: %w", err)
	}
	getInfo, err := ni.recvCS.decrypt(nil, postHS)
	if err != nil {
		emit(bridgeEvent{Type: "error", Message: fmt.Sprintf("caBLE post-handshake decrypt failed: %v", err)})
		return nil, fmt.Errorf("decrypt post-handshake message: %w", err)
	}
	if cfg.Dump {
		c.log("cable: post-handshake getInfo message (%d bytes): %x", len(getInfo), getInfo)
	}
	emit(bridgeEvent{Type: "login", Event: "passkey-cable-ready"})

	// 5. CTAP2 getAssertion.
	clientDataJSON, err := buildClientDataJSON(pub.Challenge, cfg.Origin)
	if err != nil {
		return nil, err
	}
	cmd, err := buildGetAssertionCommand(pub, sha256Sum(clientDataJSON))
	if err != nil {
		return nil, err
	}
	framed := append([]byte{cableMsgTypeCTAP}, cmd...)
	ctapCipher, err := ni.sendCS.encrypt(nil, framed)
	if err != nil {
		return nil, err
	}
	if err := conn.write(ctx, ctapCipher); err != nil {
		return nil, fmt.Errorf("send CTAP getAssertion: %w", err)
	}
	respCipher, err := conn.read(ctx)
	if err != nil {
		return nil, fmt.Errorf("read CTAP response: %w", err)
	}
	respPlain, err := ni.recvCS.decrypt(nil, respCipher)
	if err != nil {
		return nil, fmt.Errorf("decrypt CTAP response: %w", err)
	}
	// Strip the leading MessageType byte (kCTAP) before CBOR parsing.
	if len(respPlain) > 0 {
		if respPlain[0] != cableMsgTypeCTAP && cfg.Dump {
			c.log("cable: WARNING unexpected reply message type %#x", respPlain[0])
		}
		respPlain = respPlain[1:]
	}
	assertion, err := parseGetAssertionResponse(respPlain)
	if err != nil {
		emit(bridgeEvent{Type: "error", Message: fmt.Sprintf("caBLE CTAP getAssertion rejected: %v", err)})
		return nil, err
	}
	c.log("cable: getAssertion succeeded credential_id=%x numCreds=%d",
		assertion.CredentialID, assertion.NumberOfCredentials)
	// For allowCredentials=[] the phone may hold several discoverable
	// credentials. WhatsApp's challenge (observed) yields one, so the first
	// assertion is used; if more are ever returned, surface it rather than
	// silently guessing (getNextAssertion selection would go here).
	if assertion.NumberOfCredentials > 1 {
		c.log("cable: WARNING phone returned %d credentials; using the first (getNextAssertion selection not implemented)", assertion.NumberOfCredentials)
	}
	emit(bridgeEvent{Type: "login", Event: "passkey-cable-assertion-received"})

	return assertion.toWebAuthnResponse(clientDataJSON), nil
}
