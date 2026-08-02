//go:build darwin || freebsd || netbsd || openbsd || dragonfly

package main

import (
	"syscall"

	"golang.org/x/sys/unix"
)

func accountBridgeBootstrapDescriptor() uintptr { return accountBridgeBootstrapFD }

func accountBridgeDescriptorIsOpen(fd uintptr) bool {
	var stat syscall.Stat_t
	if syscall.Fstat(int(fd), &stat) != nil || stat.Mode&syscall.S_IFMT != syscall.S_IFIFO {
		return false
	}
	flags, err := unix.FcntlInt(fd, unix.F_GETFL, 0)
	if err != nil || flags&unix.O_ACCMODE != unix.O_RDONLY {
		return false
	}
	// With FD 3 absent, Go's runtime may reuse descriptor 3 for its kqueue.
	// A zero-time kevent succeeds only for a kqueue; reject it before os.NewFile
	// can register or close a runtime-owned descriptor.
	if _, err = unix.Kevent(int(fd), nil, nil, &unix.Timespec{}); err == nil {
		return false
	}
	return true
}
