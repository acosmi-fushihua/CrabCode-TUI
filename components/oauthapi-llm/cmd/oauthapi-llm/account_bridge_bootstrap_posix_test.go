//go:build !windows

package main

import (
	"os"
	"testing"

	"golang.org/x/sys/unix"
)

// The production supervisor is a Bun process. Bun's `child_process` backs every
// extra `stdio` entry with a socketpair(2), so the sidecar receives FD 3 as
// S_IFSOCK with O_RDWR — never the S_IFIFO/O_RDONLY that `os.Pipe()` produces.
// TestReadAccountBridgeBootstrapRequiresReadOnlyPipe drives an os.Pipe and is
// therefore structurally unable to observe the real hand-off; it stayed green
// through every repair round while every POSIX install failed closed at
// startup. This test drives the descriptor shape production actually creates.
func TestReadAccountBridgeBootstrapAcceptsSupervisorSocketpair(t *testing.T) {
	raw, trustRoot := signedBootstrapForTest(t)

	fds, err := unix.Socketpair(unix.AF_UNIX, unix.SOCK_STREAM, 0)
	if err != nil {
		t.Fatalf("socketpair: %v", err)
	}
	child := os.NewFile(uintptr(fds[0]), "bootstrap-child")
	parent := os.NewFile(uintptr(fds[1]), "bootstrap-parent")
	defer child.Close()

	done := make(chan error, 1)
	go func() {
		_, writeErr := parent.Write(raw)
		// The host writes then calls `.end()`, which shuts the write side down
		// so the sidecar's io.ReadAll observes EOF instead of blocking.
		closeErr := parent.Close()
		if writeErr != nil {
			done <- writeErr
			return
		}
		done <- closeErr
	}()

	bootstrap, err := readAccountBridgeBootstrapFD(child.Fd(), trustRoot)
	if err != nil {
		t.Fatalf("supervisor-shaped socketpair bootstrap was rejected: %v", err)
	}
	clearSecret(bootstrap.MasterKey)
	if err = <-done; err != nil {
		t.Fatalf("write socketpair bootstrap: %v", err)
	}
}

// The relaxation above must not accept a descriptor we could never legitimately
// read the bootstrap from. A write-only FIFO is the exact end of the pipe the
// parent keeps, so it stands in for "handed the wrong end".
func TestReadAccountBridgeBootstrapRejectsWriteOnlyDescriptor(t *testing.T) {
	_, trustRoot := signedBootstrapForTest(t)

	reader, writer, err := os.Pipe()
	if err != nil {
		t.Fatalf("create pipe: %v", err)
	}
	defer reader.Close()
	defer writer.Close()

	if _, err = readAccountBridgeBootstrapFD(writer.Fd(), trustRoot); err == nil {
		t.Fatal("write-only pipe end was accepted as the private bootstrap channel")
	}
}
