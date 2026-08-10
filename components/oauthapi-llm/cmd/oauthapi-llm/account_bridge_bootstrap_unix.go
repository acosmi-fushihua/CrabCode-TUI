//go:build !windows && !darwin && !freebsd && !netbsd && !openbsd && !dragonfly

package main

import (
	"syscall"

	"golang.org/x/sys/unix"
)

func accountBridgeBootstrapDescriptor() uintptr { return accountBridgeBootstrapFD }

// accountBridgeDescriptorIsOpen reports whether FD 3 is the private bootstrap
// channel the supervisor handed us.
//
// It must accept BOTH descriptor kinds a legitimate parent can produce. A Go
// parent (`exec.Cmd.ExtraFiles`) passes a `pipe(2)` — S_IFIFO, O_RDONLY. The
// CrabCode supervisor is a Bun process, and Bun's `child_process` backs every
// extra `stdio` entry with a `socketpair(2)`, so FD 3 arrives S_IFSOCK,
// O_RDWR. Demanding S_IFIFO/O_RDONLY therefore rejected the real supervisor on
// every POSIX install while Windows (a named pipe, FILE_TYPE_PIPE) worked —
// the account bridge failed closed at startup for every macOS and Linux user.
//
// This is not a trust boundary and never was: confidentiality and integrity of
// the bootstrap come from the Ed25519-signed grant plus the exact client
// binding, and anything able to choose our inherited descriptors already
// controls our process creation. The check exists so we never hand a
// runtime-owned descriptor to os.NewFile; a write-only descriptor and every
// other file type still fail closed.
func accountBridgeDescriptorIsOpen(fd uintptr) bool {
	var stat syscall.Stat_t
	if syscall.Fstat(int(fd), &stat) != nil {
		return false
	}
	if kind := stat.Mode & syscall.S_IFMT; kind != syscall.S_IFIFO && kind != syscall.S_IFSOCK {
		return false
	}
	flags, err := unix.FcntlInt(fd, unix.F_GETFL, 0)
	return err == nil && flags&unix.O_ACCMODE != unix.O_WRONLY
}
