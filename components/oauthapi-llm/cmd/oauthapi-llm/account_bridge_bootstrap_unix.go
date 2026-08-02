//go:build !windows && !darwin && !freebsd && !netbsd && !openbsd && !dragonfly

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
	return err == nil && flags&unix.O_ACCMODE == unix.O_RDONLY
}
