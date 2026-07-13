package main

import (
	"os"
	"strconv"
	"time"
)

// cableConfig holds all caBLE ceremony knobs, populated from environment
// variables. This mirrors the existing CHATCLI_WHATSAPP_PASSKEY_ORIGIN override
// pattern (passkey.go:54) so the live ceremony can be iterated without rebuilds.
type cableConfig struct {
	// Disable falls back to the legacy self-mint assertion path for A/B testing.
	Disable bool
	// TunnelDomain, when set, forces the tunnel server domain (bypasses the
	// advert-derived index).
	TunnelDomain string
	// BLEAdapter is the hciN adapter to scan on (default "hci0").
	BLEAdapter string
	// Timeout bounds the whole ceremony.
	Timeout time.Duration
	// Linger bounds how long the tunnel is kept open AFTER the assertion.
	// Default 0 (close immediately): a live A/B on 2026-07-04 showed that
	// holding the tunnel open makes the phone wait ~27s, close with an abrupt
	// EOF, and NEVER send the pairing continuation, while the immediate close
	// used by both successful 2026-07-03 pairings let pairing complete. Set
	// CHATCLI_WHATSAPP_CABLE_LINGER=<secs> to re-run that experiment.
	Linger time.Duration
	// Dump enables raw frame hex-dumping to the debug log.
	Dump bool
	// Origin overrides the WebAuthn origin in clientDataJSON.
	Origin string
}

func loadCableConfig() cableConfig {
	cfg := cableConfig{
		Disable:      os.Getenv("CHATCLI_WHATSAPP_CABLE_DISABLE") != "",
		TunnelDomain: os.Getenv("CHATCLI_WHATSAPP_CABLE_TUNNEL_DOMAIN"),
		BLEAdapter:   os.Getenv("CHATCLI_WHATSAPP_CABLE_BLE_ADAPTER"),
		Dump:         os.Getenv("CHATCLI_WHATSAPP_CABLE_DUMP") != "",
		Origin:       passkeyOrigin(),
		Timeout:      3 * time.Minute,
		Linger:       0,
	}
	if cfg.BLEAdapter == "" {
		cfg.BLEAdapter = "hci0"
	}
	if v := os.Getenv("CHATCLI_WHATSAPP_CABLE_TIMEOUT"); v != "" {
		if secs, err := strconv.Atoi(v); err == nil && secs > 0 {
			cfg.Timeout = time.Duration(secs) * time.Second
		}
	}
	if v := os.Getenv("CHATCLI_WHATSAPP_CABLE_LINGER"); v != "" {
		if secs, err := strconv.Atoi(v); err == nil && secs >= 0 {
			cfg.Linger = time.Duration(secs) * time.Second
		}
	}
	return cfg
}
