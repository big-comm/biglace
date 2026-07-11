package tsbridge

import (
	"net"
	"testing"
)

func TestStartValidatesRequiredFields(t *testing.T) {
	if err := Start("", "", "phone", t.TempDir()); err == nil {
		t.Fatal("Start accepted an empty control URL")
	}
	if err := Start("https://example.invalid", "", "phone", ""); err == nil {
		t.Fatal("Start accepted an empty state directory")
	}
}

func TestSetInterfacesReplacesSnapshot(t *testing.T) {
	err := SetInterfaces(`[{"name":"wlan0","index":4,"mtu":1500,"up":true,"addrs":["192.0.2.4/24","bad"]}]`)
	if err != nil {
		t.Fatalf("SetInterfaces failed: %v", err)
	}
	interfaceMu.RLock()
	defer interfaceMu.RUnlock()
	if len(interfaces) != 1 || interfaces[0].Interface.Name != "wlan0" {
		t.Fatalf("unexpected interface snapshot: %#v", interfaces)
	}
	if interfaces[0].Interface.Flags&net.FlagUp == 0 || len(interfaces[0].AltAddrs) != 1 {
		t.Fatalf("flags/addrs not converted: %#v", interfaces[0])
	}
}

func TestSetInterfacesRejectsInvalidJSON(t *testing.T) {
	if err := SetInterfaces("{"); err == nil {
		t.Fatal("SetInterfaces accepted invalid JSON")
	}
}
