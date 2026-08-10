//go:build darwin || freebsd || netbsd || openbsd || dragonfly

package main

import (
	"syscall"

	"golang.org/x/sys/unix"
)

func accountBridgeBootstrapDescriptor() uintptr { return accountBridgeBootstrapFD }

// accountBridgeDescriptorIsOpen mirrors the generic POSIX probe (see
// account_bridge_bootstrap_unix.go for why both S_IFIFO and S_IFSOCK must be
// accepted: Bun's child_process backs extra stdio entries with a socketpair,
// so the real supervisor hands us S_IFSOCK/O_RDWR) and keeps the Darwin-only
// kqueue rejection below.
func accountBridgeDescriptorIsOpen(fd uintptr) bool {
	var stat syscall.Stat_t
	if syscall.Fstat(int(fd), &stat) != nil {
		return false
	}
	if kind := stat.Mode & syscall.S_IFMT; kind != syscall.S_IFIFO && kind != syscall.S_IFSOCK {
		return false
	}
	flags, err := unix.FcntlInt(fd, unix.F_GETFL, 0)
	if err != nil || flags&unix.O_ACCMODE == unix.O_WRONLY {
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
