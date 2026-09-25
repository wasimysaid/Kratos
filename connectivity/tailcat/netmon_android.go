//go:build android && cgo

package tailcatnative

/*
#include <arpa/inet.h>
#include <ifaddrs.h>
#include <net/if.h>
#include <netinet/in.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>

struct kratos_ifaddr {
	char name[IF_NAMESIZE];
	unsigned int flags;
	unsigned int index;
	int family;
	uint8_t address[16];
	uint8_t prefix;
};

static uint8_t kratos_prefix(const uint8_t *mask, size_t n) {
	uint8_t prefix = 0;
	for (size_t i = 0; i < n; i++) {
		uint8_t b = mask[i];
		while (b != 0) {
			prefix += b & 1;
			b >>= 1;
		}
	}
	return prefix;
}

static int kratos_getifaddrs(struct kratos_ifaddr **out) {
	struct ifaddrs *head = NULL;
	if (getifaddrs(&head) != 0) return -1;

	size_t count = 0;
	for (struct ifaddrs *ifa = head; ifa != NULL; ifa = ifa->ifa_next) {
		if (ifa->ifa_name != NULL && ifa->ifa_addr != NULL &&
			(ifa->ifa_addr->sa_family == AF_INET || ifa->ifa_addr->sa_family == AF_INET6)) {
			count++;
		}
	}
	struct kratos_ifaddr *items = calloc(count ? count : 1, sizeof(*items));
	if (items == NULL) { freeifaddrs(head); return -1; }

	size_t i = 0;
	for (struct ifaddrs *ifa = head; ifa != NULL; ifa = ifa->ifa_next) {
		if (ifa->ifa_name == NULL || ifa->ifa_addr == NULL) continue;
		int family = ifa->ifa_addr->sa_family;
		if (family != AF_INET && family != AF_INET6) continue;
		struct kratos_ifaddr *item = &items[i++];
		strncpy(item->name, ifa->ifa_name, IF_NAMESIZE - 1);
		item->flags = ifa->ifa_flags;
		item->index = if_nametoindex(ifa->ifa_name);
		item->family = family;
		if (family == AF_INET) {
			struct sockaddr_in *addr = (struct sockaddr_in *)ifa->ifa_addr;
			memcpy(item->address, &addr->sin_addr, 4);
			if (ifa->ifa_netmask != NULL) {
				struct sockaddr_in *mask = (struct sockaddr_in *)ifa->ifa_netmask;
				item->prefix = kratos_prefix((uint8_t *)&mask->sin_addr, 4);
			}
		} else {
			struct sockaddr_in6 *addr = (struct sockaddr_in6 *)ifa->ifa_addr;
			memcpy(item->address, &addr->sin6_addr, 16);
			if (ifa->ifa_netmask != NULL) {
				struct sockaddr_in6 *mask = (struct sockaddr_in6 *)ifa->ifa_netmask;
				item->prefix = kratos_prefix((uint8_t *)&mask->sin6_addr, 16);
			}
		}
	}
	freeifaddrs(head);
	*out = items;
	return (int)i;
}

static void kratos_freeifaddrs(struct kratos_ifaddr *items) { free(items); }
*/
import "C"

import (
	"net"
	"unsafe"

	"tailscale.com/net/netmon"
)

func init() {
	netmon.RegisterInterfaceGetter(androidInterfaces)
}

// androidInterfaces avoids Go's Android netlink route query, which is denied
// to normal Android applications. bionic implements getifaddrs via the
// permitted SIOCGIFCONF socket ioctl path.
func androidInterfaces() ([]netmon.Interface, error) {
	if interfaces, err := net.Interfaces(); err == nil && len(interfaces) > 0 {
		return wrapInterfaces(interfaces), nil
	}
	return androidGetifaddrs()
}

func wrapInterfaces(interfaces []net.Interface) []netmon.Interface {
	result := make([]netmon.Interface, len(interfaces))
	for i := range interfaces {
		result[i].Interface = &interfaces[i]
	}
	return result
}

func androidGetifaddrs() ([]netmon.Interface, error) {
	var rows *C.struct_kratos_ifaddr
	count := C.kratos_getifaddrs(&rows)
	if count < 0 {
		return nil, &net.OpError{Op: "getifaddrs", Net: "ip"}
	}
	defer C.kratos_freeifaddrs(rows)

	byName := make(map[string]*netmon.Interface)
	for _, row := range unsafe.Slice(rows, int(count)) {
		name := C.GoString(&row.name[0])
		if name == "" {
			continue
		}
		entry := byName[name]
		if entry == nil {
			entry = &netmon.Interface{Interface: &net.Interface{
				Index: int(row.index), Name: name, Flags: androidInterfaceFlags(row.flags),
			}}
			byName[name] = entry
		}
		bits := 128
		if row.family == C.AF_INET {
			bits = 32
		}
		address := make(net.IP, bits/8)
		copy(address, unsafe.Slice((*byte)(unsafe.Pointer(&row.address[0])), len(address)))
		entry.AltAddrs = append(entry.AltAddrs, &net.IPNet{
			IP: address, Mask: net.CIDRMask(int(row.prefix), bits),
		})
	}

	result := make([]netmon.Interface, 0, len(byName))
	for _, entry := range byName {
		result = append(result, *entry)
	}
	return result, nil
}

func androidInterfaceFlags(flags C.uint) net.Flags {
	var result net.Flags
	if flags&C.IFF_UP != 0 {
		result |= net.FlagUp
	}
	if flags&C.IFF_BROADCAST != 0 {
		result |= net.FlagBroadcast
	}
	if flags&C.IFF_LOOPBACK != 0 {
		result |= net.FlagLoopback
	}
	if flags&C.IFF_POINTOPOINT != 0 {
		result |= net.FlagPointToPoint
	}
	if flags&C.IFF_MULTICAST != 0 {
		result |= net.FlagMulticast
	}
	return result
}
