//go:build windows

package fileperm

import (
	"os"
	"path/filepath"
	"testing"

	"golang.org/x/sys/windows"
)

func TestProtectPrivatePathsReplacesInheritedWindowsACL(t *testing.T) {
	directory := filepath.Join(t.TempDir(), "private")
	if err := os.Mkdir(directory, 0o700); err != nil {
		t.Fatal(err)
	}
	file := filepath.Join(directory, "secret.json")
	if err := os.WriteFile(file, []byte("fixture"), 0o600); err != nil {
		t.Fatal(err)
	}

	if err := ProtectDirectory(directory); err != nil {
		t.Fatal(err)
	}
	if err := ProtectFile(file); err != nil {
		t.Fatal(err)
	}
	if err := ValidatePrivateDirectory(directory); err != nil {
		t.Fatal(err)
	}
	if err := ValidatePrivateFile(file); err != nil {
		t.Fatal(err)
	}
}

func TestValidatePrivateFileRejectsAdditionalWindowsPrincipal(t *testing.T) {
	file := filepath.Join(t.TempDir(), "secret.json")
	if err := os.WriteFile(file, []byte("fixture"), 0o600); err != nil {
		t.Fatal(err)
	}
	current, err := currentUserSID()
	if err != nil {
		t.Fatal(err)
	}
	everyone, err := windows.StringToSid("S-1-1-0")
	if err != nil {
		t.Fatal(err)
	}
	acl, err := windows.ACLFromEntries([]windows.EXPLICIT_ACCESS{
		{
			AccessPermissions: windows.ACCESS_MASK(windows.GENERIC_ALL),
			AccessMode:        windows.SET_ACCESS,
			Trustee:           windows.TRUSTEE{TrusteeForm: windows.TRUSTEE_IS_SID, TrusteeType: windows.TRUSTEE_IS_USER, TrusteeValue: windows.TrusteeValueFromSID(current)},
		},
		{
			AccessPermissions: windows.ACCESS_MASK(windows.GENERIC_READ),
			AccessMode:        windows.GRANT_ACCESS,
			Trustee:           windows.TRUSTEE{TrusteeForm: windows.TRUSTEE_IS_SID, TrusteeType: windows.TRUSTEE_IS_GROUP, TrusteeValue: windows.TrusteeValueFromSID(everyone)},
		},
	}, nil)
	if err != nil {
		t.Fatal(err)
	}
	if err = windows.SetNamedSecurityInfo(file, windows.SE_FILE_OBJECT, windows.DACL_SECURITY_INFORMATION|windows.PROTECTED_DACL_SECURITY_INFORMATION, nil, nil, acl, nil); err != nil {
		t.Fatal(err)
	}
	if err = ValidatePrivateFile(file); err == nil {
		t.Fatal("DACL granting Everyone was accepted")
	}
}
