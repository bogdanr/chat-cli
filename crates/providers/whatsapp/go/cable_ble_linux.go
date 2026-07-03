//go:build linux

package main

// BlueZ BLE scanner (Milestone 2, Linux only).
//
// Scans for the phone's caBLE advertisement over D-Bus (org.bluez), extracts
// the 20-byte encrypted EID from the FIDO service data (UUID 0xFFF9), and
// decrypts it with the QR-derived EID key to obtain the tunnel routing info.
//
// Uses BlueZ over the system D-Bus rather than raw HCI sockets, so it needs no
// CAP_NET_ADMIN — only D-Bus policy access to org.bluez (see plan Risk 1).

import (
	"context"
	"fmt"
	"strings"
	"time"

	"github.com/godbus/dbus/v5"
)

const (
	bluezService      = "org.bluez"
	bluezAdapterIface = "org.bluez.Adapter1"
	bluezDeviceIface  = "org.bluez.Device1"
	// Full 128-bit form of the 16-bit FIDO caBLE service UUID 0xFFF9.
	cableServiceUUID = "0000fff9-0000-1000-8000-00805f9b34fb"
)

// scanCableAdvert discovers and decrypts the phone's caBLE advert. It returns
// the decoded EID components and the raw advert bytes.
func scanCableAdvert(ctx context.Context, cfg cableConfig, eidKey []byte, logf func(string, ...any)) (*eidComponents, []byte, error) {
	conn, err := dbus.ConnectSystemBus()
	if err != nil {
		return nil, nil, fmt.Errorf("connect system bus (is bluetoothd running?): %w", err)
	}
	defer conn.Close()

	adapterPath := dbus.ObjectPath("/org/bluez/" + cfg.BLEAdapter)
	adapter := conn.Object(bluezService, adapterPath)

	// Ensure the adapter is powered.
	if err := adapter.SetProperty(bluezAdapterIface+".Powered", dbus.MakeVariant(true)); err != nil {
		logf("cable BLE: could not set Powered on %s: %v", cfg.BLEAdapter, err)
	}

	if call := adapter.Call(bluezAdapterIface+".StartDiscovery", 0); call.Err != nil {
		return nil, nil, fmt.Errorf("start BLE discovery on %s: %w", cfg.BLEAdapter, call.Err)
	}
	defer adapter.Call(bluezAdapterIface+".StopDiscovery", 0)

	// Subscribe to PropertiesChanged and InterfacesAdded so we see adverts.
	if err := conn.AddMatchSignal(
		dbus.WithMatchInterface("org.freedesktop.DBus.Properties"),
		dbus.WithMatchMember("PropertiesChanged"),
	); err != nil {
		return nil, nil, fmt.Errorf("add PropertiesChanged match: %w", err)
	}
	if err := conn.AddMatchSignal(
		dbus.WithMatchInterface("org.freedesktop.DBus.ObjectManager"),
		dbus.WithMatchMember("InterfacesAdded"),
	); err != nil {
		return nil, nil, fmt.Errorf("add InterfacesAdded match: %w", err)
	}

	signals := make(chan *dbus.Signal, 32)
	conn.Signal(signals)

	// Poll any already-known devices first.
	if comp, raw, ok := scanExistingDevices(conn, eidKey, cfg.Dump, logf); ok {
		return comp, raw, nil
	}

	deadline := time.NewTimer(cfg.Timeout)
	defer deadline.Stop()

	for {
		select {
		case <-ctx.Done():
			return nil, nil, ctx.Err()
		case <-deadline.C:
			return nil, nil, fmt.Errorf("BLE scan timed out after %s (phone did not advertise)", cfg.Timeout)
		case sig := <-signals:
			advert := advertFromSignal(sig)
			if advert == nil {
				continue
			}
			if cfg.Dump {
				logf("cable BLE: candidate advert %x", advert)
			}
			comp, err := decryptAdvert(advert, eidKey)
			if err != nil {
				// Not addressed to us or wrong length; keep scanning.
				continue
			}
			if cfg.Dump {
				logf("cable BLE: decrypted advert routing=%x domain=%d nonce=%x",
					comp.RoutingID, comp.TunnelDomain, comp.Nonce)
			}
			return comp, advert, nil
		}
	}
}

// advertFromSignal extracts a caBLE-sized service-data payload from a D-Bus
// signal, if present.
func advertFromSignal(sig *dbus.Signal) []byte {
	switch {
	case strings.HasSuffix(sig.Name, "PropertiesChanged"):
		if len(sig.Body) < 2 {
			return nil
		}
		iface, _ := sig.Body[0].(string)
		if iface != bluezDeviceIface {
			return nil
		}
		changed, ok := sig.Body[1].(map[string]dbus.Variant)
		if !ok {
			return nil
		}
		return serviceDataAdvert(changed)
	case strings.HasSuffix(sig.Name, "InterfacesAdded"):
		if len(sig.Body) < 2 {
			return nil
		}
		ifaces, ok := sig.Body[1].(map[string]map[string]dbus.Variant)
		if !ok {
			return nil
		}
		if props, ok := ifaces[bluezDeviceIface]; ok {
			return serviceDataAdvert(props)
		}
	}
	return nil
}

// serviceDataAdvert pulls the caBLE service-data bytes from a Device1 property
// map.
func serviceDataAdvert(props map[string]dbus.Variant) []byte {
	v, ok := props["ServiceData"]
	if !ok {
		return nil
	}
	sd, ok := v.Value().(map[string]dbus.Variant)
	if !ok {
		return nil
	}
	entry, ok := sd[cableServiceUUID]
	if !ok {
		return nil
	}
	data, ok := entry.Value().([]byte)
	if !ok || len(data) != bleAdvertSize {
		return nil
	}
	return data
}

// scanExistingDevices checks devices already known to BlueZ for a caBLE advert.
func scanExistingDevices(conn *dbus.Conn, eidKey []byte, dump bool, logf func(string, ...any)) (*eidComponents, []byte, bool) {
	obj := conn.Object(bluezService, "/")
	var objects map[dbus.ObjectPath]map[string]map[string]dbus.Variant
	if err := obj.Call("org.freedesktop.DBus.ObjectManager.GetManagedObjects", 0).Store(&objects); err != nil {
		return nil, nil, false
	}
	for _, ifaces := range objects {
		props, ok := ifaces[bluezDeviceIface]
		if !ok {
			continue
		}
		advert := serviceDataAdvert(props)
		if advert == nil {
			continue
		}
		if comp, err := decryptAdvert(advert, eidKey); err == nil {
			if dump {
				logf("cable BLE: matched existing device advert %x", advert)
			}
			return comp, advert, true
		}
	}
	return nil, nil, false
}
