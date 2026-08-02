//go:build !windows

package fileperm

import (
	"os"
	"path/filepath"
	"testing"
)

func TestProtectPrivatePathsEnforcesUnixModes(t *testing.T) {
	directory := filepath.Join(t.TempDir(), "private")
	if err := os.Mkdir(directory, 0o755); err != nil {
		t.Fatal(err)
	}
	file := filepath.Join(directory, "secret.json")
	if err := os.WriteFile(file, []byte("fixture"), 0o644); err != nil {
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

func TestValidatePrivatePathsRejectsRelaxedUnixModes(t *testing.T) {
	directory := filepath.Join(t.TempDir(), "private")
	if err := os.Mkdir(directory, 0o755); err != nil {
		t.Fatal(err)
	}
	file := filepath.Join(directory, "secret.json")
	if err := os.WriteFile(file, []byte("fixture"), 0o644); err != nil {
		t.Fatal(err)
	}
	if err := ValidatePrivateDirectory(directory); err == nil {
		t.Fatal("relaxed directory mode was accepted")
	}
	if err := ValidatePrivateFile(file); err == nil {
		t.Fatal("relaxed file mode was accepted")
	}
}
