//go:build windows

package main

import "syscall"

// Windows does not permit a parent to choose the numeric value of an inherited
// HANDLE. The fixed private slot is therefore STD_INPUT_HANDLE; the supervisor
// must replace it with the read end of an anonymous pipe.
func accountBridgeBootstrapDescriptor() uintptr {
	handle, err := syscall.GetStdHandle(syscall.STD_INPUT_HANDLE)
	if err != nil || handle == syscall.InvalidHandle {
		return 0
	}
	return uintptr(handle)
}

func accountBridgeDescriptorIsOpen(handle uintptr) bool {
	fileType, err := syscall.GetFileType(syscall.Handle(handle))
	return err == nil && fileType == syscall.FILE_TYPE_PIPE
}
