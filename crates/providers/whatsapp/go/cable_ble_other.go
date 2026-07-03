//go:build !linux

package main

// Non-Linux stub for the BLE scanner. caBLE hybrid linking is only supported on
// Linux/BlueZ for now; other platforms return a clear unsupported error so the
// c-archive still builds for macOS/Windows (see build.rs).

import (
	"context"
	"fmt"
)

func scanCableAdvert(ctx context.Context, cfg cableConfig, eidKey []byte, logf func(string, ...any)) (*eidComponents, []byte, error) {
	return nil, nil, fmt.Errorf("caBLE hybrid passkey linking is only supported on Linux/BlueZ")
}
