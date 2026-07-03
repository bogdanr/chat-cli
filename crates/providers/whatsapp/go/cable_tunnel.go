package main

// caBLE tunnel-server connection (Milestone 3).
//
// The BLE advert yields an encoded tunnel-domain value, a routing ID, and a
// nonce. From these the client derives a tunnel ID and opens a WebSocket to the
// assigned tunnel server, over which the Noise handshake and CTAP2 traffic run.

import (
	"context"
	"crypto/sha256"
	"encoding/binary"
	"fmt"
	"net/http"
	"strings"

	"github.com/coder/websocket"
)

// assignedTunnelDomains is the caBLE "assigned" domain table. Small encoded
// values index directly into it. The two entries are confirmed reachable from
// this host (cable.ua5v.com, cable.auth.com). Larger encoded values (>=256) use
// the hashed derivation (decodeTunnelDomain) and can be overridden via config.
var assignedTunnelDomains = []string{
	"cable.ua5v.com",
	"cable.auth.com",
}

// tunnelDomainTLDs is the hashed-domain top-level-domain table, selected by the
// low two bits of the SHA-256 digest. Order is significant and mirrors
// Chromium's tunnelserver::DecodeDomain.
var tunnelDomainTLDs = []string{"com", "org", "net", "info"}

// tunnelDomainHashPrefix is the 28-byte salt Chromium prepends before the
// little-endian domain value and a trailing NUL when hashing a non-assigned
// tunnel-domain identifier.
const tunnelDomainHashPrefix = "caBLEv2 tunnel server domain"

// tunnelDomainBase32 is the lowercase RFC 4648 base32 alphabet used to render
// the hashed label, five bits at a time.
const tunnelDomainBase32 = "abcdefghijklmnopqrstuvwxyz234567"

// decodeTunnelDomain maps an encoded 16-bit tunnel-domain value to a hostname,
// byte-for-byte matching Chromium's tunnelserver::DecodeDomain:
//
//   - values < len(assignedTunnelDomains): index the assigned table directly;
//   - values in [len(assignedTunnelDomains), 256): unassigned/invalid;
//   - values >= 256: hashed. SHA-256 over
//     [28-byte prefix][2-byte domain LE][1 NUL]; take the first 8 digest bytes
//     as a little-endian u64; the low 2 bits select the TLD, then each
//     subsequent 5 bits emit a base32 char until the value is exhausted.
func decodeTunnelDomain(encoded uint16) (string, error) {
	if int(encoded) < len(assignedTunnelDomains) {
		return assignedTunnelDomains[encoded], nil
	}
	if encoded < 256 {
		return "", fmt.Errorf("unassigned tunnel domain %d (no hashed form below 256)", encoded)
	}

	templ := make([]byte, 31)
	copy(templ, tunnelDomainHashPrefix) // 28 bytes
	binary.LittleEndian.PutUint16(templ[28:30], encoded)
	// templ[30] stays NUL.

	digest := sha256.Sum256(templ)
	result := binary.LittleEndian.Uint64(digest[:8])

	tld := result & 3
	result >>= 2

	var sb strings.Builder
	sb.WriteString("cable.")
	for result != 0 {
		sb.WriteByte(tunnelDomainBase32[result&31])
		result >>= 5
	}
	sb.WriteByte('.')
	sb.WriteString(tunnelDomainTLDs[tld])
	return sb.String(), nil
}

// tunnelConn wraps the WebSocket to the tunnel server.
type tunnelConn struct {
	ws     *websocket.Conn
	domain string
}

// dialTunnel opens the caBLE tunnel WebSocket:
//
//	wss://<domain>/cable/connect/<routing-id-hex>/<tunnel-id-hex>
//
// with the required "fido.cable" subprotocol.
func dialTunnel(ctx context.Context, domain string, routingID, tunnelID []byte) (*tunnelConn, error) {
	url := fmt.Sprintf("wss://%s/cable/connect/%s/%s",
		domain, toHex(routingID), toHex(tunnelID))
	ws, _, err := websocket.Dial(ctx, url, &websocket.DialOptions{
		Subprotocols: []string{cableWebSocketSubprotocol},
		HTTPHeader:   http.Header{},
	})
	if err != nil {
		return nil, fmt.Errorf("dial tunnel %s: %w", domain, err)
	}
	return &tunnelConn{ws: ws, domain: domain}, nil
}

func (t *tunnelConn) write(ctx context.Context, data []byte) error {
	return t.ws.Write(ctx, websocket.MessageBinary, data)
}

func (t *tunnelConn) read(ctx context.Context) ([]byte, error) {
	_, data, err := t.ws.Read(ctx)
	return data, err
}

func (t *tunnelConn) close() {
	if t.ws != nil {
		_ = t.ws.Close(websocket.StatusNormalClosure, "")
	}
}

func toHex(b []byte) string {
	// Uppercase to mirror Chromium's base::HexEncode. The tunnel id is the
	// rendezvous key both the phone and this client must present to the tunnel
	// server identically, so the encoding case must match the reference.
	const hexdigits = "0123456789ABCDEF"
	var sb strings.Builder
	for _, c := range b {
		sb.WriteByte(hexdigits[c>>4])
		sb.WriteByte(hexdigits[c&0xf])
	}
	return sb.String()
}
