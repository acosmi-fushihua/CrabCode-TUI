//go:build !windows

// Package fileperm enforces the Account Bridge's private on-disk boundary.
package fileperm

import (
	"fmt"
	"os"
)

const (
	privateDirectoryMode = 0o700
	privateFileMode      = 0o600
)

// ProtectDirectory removes group/other access and verifies a real directory.
func ProtectDirectory(path string) error {
	if err := validateKind(path, true); err != nil {
		return err
	}
	if err := os.Chmod(path, privateDirectoryMode); err != nil {
		return fmt.Errorf("protect private directory: %w", err)
	}
	return ValidatePrivateDirectory(path)
}

// ProtectFile removes group/other access and verifies a regular file.
func ProtectFile(path string) error {
	if err := validateKind(path, false); err != nil {
		return err
	}
	if err := os.Chmod(path, privateFileMode); err != nil {
		return fmt.Errorf("protect private file: %w", err)
	}
	return ValidatePrivateFile(path)
}

// ValidatePrivateDirectory requires an exact Unix 0700 directory mode.
func ValidatePrivateDirectory(path string) error {
	if err := validateKind(path, true); err != nil {
		return err
	}
	info, err := os.Lstat(path)
	if err != nil {
		return err
	}
	if info.Mode().Perm() != privateDirectoryMode {
		return fmt.Errorf("private directory mode must be 0700, got %04o", info.Mode().Perm())
	}
	return nil
}

// ValidatePrivateFile requires an exact Unix 0600 regular-file mode.
func ValidatePrivateFile(path string) error {
	if err := validateKind(path, false); err != nil {
		return err
	}
	info, err := os.Lstat(path)
	if err != nil {
		return err
	}
	if info.Mode().Perm() != privateFileMode {
		return fmt.Errorf("private file mode must be 0600, got %04o", info.Mode().Perm())
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
