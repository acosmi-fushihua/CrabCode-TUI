//go:build windows

// Package fileperm enforces the Account Bridge's private on-disk boundary.
package fileperm

import (
	"fmt"
	"os"
	"runtime"
	"unsafe"

	"golang.org/x/sys/windows"
)

const (
	aclRevision         = byte(2)
	fileAllAccessMask   = windows.STANDARD_RIGHTS_REQUIRED | windows.SYNCHRONIZE | 0x1ff
	privateFileACEFlags = byte(0)
)

// windows.ACL intentionally keeps its header fields private. This matching
// layout lets us build and validate a minimal ACL without SetEntriesInAcl's
// merge/canonicalization semantics adding inherited or synthetic entries.
type aclHeader struct {
	revision  byte
	reserved  byte
	size      uint16
	aceCount  uint16
	reserved2 uint16
}

// ProtectDirectory replaces inherited permissions with a protected DACL that
// grants full access only to the current process user. The ACE is inherited by
// children, and every sensitive child is independently re-checked as well.
func ProtectDirectory(path string) error {
	if err := validateKind(path, true); err != nil {
		return err
	}
	if err := protectCurrentUserOnly(path, true); err != nil {
		return err
	}
	return ValidatePrivateDirectory(path)
}

// ProtectFile replaces inherited permissions with a protected current-user
// only DACL. chmod is insufficient on Windows and is intentionally not used as
// a security boundary here.
func ProtectFile(path string) error {
	if err := validateKind(path, false); err != nil {
		return err
	}
	if err := protectCurrentUserOnly(path, false); err != nil {
		return err
	}
	return ValidatePrivateFile(path)
}

// ValidatePrivateDirectory verifies a protected, current-user-only DACL.
func ValidatePrivateDirectory(path string) error {
	if err := validateKind(path, true); err != nil {
		return err
	}
	return validateCurrentUserOnly(path, true)
}

// ValidatePrivateFile verifies a protected, current-user-only DACL.
func ValidatePrivateFile(path string) error {
	if err := validateKind(path, false); err != nil {
		return err
	}
	return validateCurrentUserOnly(path, false)
}

func currentUserSID() (*windows.SID, error) {
	user, err := windows.GetCurrentProcessToken().GetTokenUser()
	if err != nil {
		return nil, fmt.Errorf("read current Windows user SID: %w", err)
	}
	if user == nil || user.User.Sid == nil || !user.User.Sid.IsValid() {
		return nil, fmt.Errorf("current Windows user SID is invalid")
	}
	return user.User.Sid, nil
}

func protectCurrentUserOnly(path string, directory bool) error {
	sid, err := currentUserSID()
	if err != nil {
		return err
	}
	aceFlags := privateFileACEFlags
	if directory {
		aceFlags = windows.OBJECT_INHERIT_ACE | windows.CONTAINER_INHERIT_ACE
	}
	acl, backing, err := exactCurrentUserACL(sid, aceFlags)
	if err != nil {
		return err
	}
	if err = windows.SetNamedSecurityInfo(
		path,
		windows.SE_FILE_OBJECT,
		windows.DACL_SECURITY_INFORMATION|windows.PROTECTED_DACL_SECURITY_INFORMATION,
		nil,
		nil,
		acl,
		nil,
	); err != nil {
		return fmt.Errorf("apply current-user-only Windows ACL: %w", err)
	}
	runtime.KeepAlive(backing)
	return nil
}

func exactCurrentUserACL(sid *windows.SID, aceFlags byte) (*windows.ACL, []byte, error) {
	if sid == nil || !sid.IsValid() {
		return nil, nil, fmt.Errorf("build current-user-only Windows ACL: SID is invalid")
	}
	headerSize := int(unsafe.Sizeof(aclHeader{}))
	aceSIDOffset := int(unsafe.Offsetof(windows.ACCESS_ALLOWED_ACE{}.SidStart))
	aceSize := aceSIDOffset + sid.Len()
	aclSize := headerSize + aceSize
	if aceSize > int(^uint16(0)) || aclSize > int(^uint16(0)) {
		return nil, nil, fmt.Errorf("build current-user-only Windows ACL: ACL is too large")
	}
	backing := make([]byte, aclSize)
	header := (*aclHeader)(unsafe.Pointer(&backing[0]))
	header.revision = aclRevision
	header.size = uint16(aclSize)
	header.aceCount = 1

	ace := (*windows.ACCESS_ALLOWED_ACE)(unsafe.Pointer(&backing[headerSize]))
	ace.Header.AceType = windows.ACCESS_ALLOWED_ACE_TYPE
	ace.Header.AceFlags = aceFlags
	ace.Header.AceSize = uint16(aceSize)
	ace.Mask = windows.ACCESS_MASK(fileAllAccessMask)
	aceSID := (*windows.SID)(unsafe.Pointer(&ace.SidStart))
	if err := windows.CopySid(uint32(sid.Len()), aceSID, sid); err != nil {
		return nil, nil, fmt.Errorf("build current-user-only Windows ACL: copy SID: %w", err)
	}
	return (*windows.ACL)(unsafe.Pointer(header)), backing, nil
}

func validateCurrentUserOnly(path string, directory bool) error {
	sid, err := currentUserSID()
	if err != nil {
		return err
	}
	descriptor, err := windows.GetNamedSecurityInfo(
		path,
		windows.SE_FILE_OBJECT,
		windows.DACL_SECURITY_INFORMATION|windows.PROTECTED_DACL_SECURITY_INFORMATION,
	)
	if err != nil {
		return fmt.Errorf("read Windows DACL: %w", err)
	}
	control, _, err := descriptor.Control()
	if err != nil {
		return fmt.Errorf("read Windows DACL control: %w", err)
	}
	if control&windows.SE_DACL_PROTECTED == 0 {
		return fmt.Errorf("private Windows DACL must be protected from inheritance")
	}
	dacl, _, err := descriptor.DACL()
	if err != nil {
		return fmt.Errorf("read Windows DACL entries: %w", err)
	}
	if dacl == nil || dacl.AceCount != 1 {
		return fmt.Errorf("private Windows DACL must contain exactly one ACE")
	}
	header := (*aclHeader)(unsafe.Pointer(dacl))
	expectedACEFlags := privateFileACEFlags
	if directory {
		expectedACEFlags = windows.OBJECT_INHERIT_ACE | windows.CONTAINER_INHERIT_ACE
	}
	expectedACESize := int(unsafe.Offsetof(windows.ACCESS_ALLOWED_ACE{}.SidStart)) + sid.Len()
	expectedACLSize := int(unsafe.Sizeof(aclHeader{})) + expectedACESize
	if header.revision != aclRevision || header.size != uint16(expectedACLSize) {
		return fmt.Errorf("private Windows DACL header is not minimal and canonical")
	}
	var ace *windows.ACCESS_ALLOWED_ACE
	if err = windows.GetAce(dacl, 0, &ace); err != nil {
		return fmt.Errorf("read Windows DACL ACE: %w", err)
	}
	if ace == nil || ace.Header.AceType != windows.ACCESS_ALLOWED_ACE_TYPE {
		return fmt.Errorf("private Windows DACL ACE must grant access")
	}
	if ace.Header.AceFlags != expectedACEFlags || ace.Header.AceSize != uint16(expectedACESize) {
		return fmt.Errorf("private Windows DACL ACE inheritance or size is invalid")
	}
	aceSID := (*windows.SID)(unsafe.Pointer(&ace.SidStart))
	if !sid.Equals(aceSID) {
		return fmt.Errorf("private Windows DACL grants a non-current-user SID")
	}
	if uint32(ace.Mask) != uint32(fileAllAccessMask) {
		return fmt.Errorf("private Windows DACL does not grant current-user full access")
	}
	return nil
}

func validateKind(path string, directory bool) error {
	info, err := os.Lstat(path)
	if err != nil {
		return err
	}
	if info.Mode()&os.ModeSymlink != 0 {
		return fmt.Errorf("private path must not be a symbolic link")
	}
	if directory && !info.IsDir() {
		return fmt.Errorf("private path must be a directory")
	}
	if !directory && !info.Mode().IsRegular() {
		return fmt.Errorf("private path must be a regular file")
	}
	return nil
}
