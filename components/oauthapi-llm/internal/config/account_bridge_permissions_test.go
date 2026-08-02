package config

import (
	"os"
	"path/filepath"
	"testing"

	"github.com/acosmi/OAuthAPI-LLM/internal/fileperm"
)

func TestLoadAndRewriteConfigEnforcesPlatformPrivatePermissions(t *testing.T) {
	path := filepath.Join(t.TempDir(), "config.yaml")
	raw := []byte(`api-keys:
  - fixture-inference-key
remote-management:
  secret-key: fixture-management-key
`)
	if err := os.WriteFile(path, raw, 0o644); err != nil {
		t.Fatal(err)
	}
	cfg, err := LoadConfigOptional(path, false)
	if err != nil {
		t.Fatal(err)
	}
	if err = fileperm.ValidatePrivateFile(path); err != nil {
		t.Fatalf("loaded config is not private: %v", err)
	}
	if err = SaveConfigPreserveComments(path, cfg); err != nil {
		t.Fatal(err)
	}
	if err = fileperm.ValidatePrivateFile(path); err != nil {
		t.Fatalf("rewritten config is not private: %v", err)
	}
}

func TestLoadConfigOptionalDirectoryRemainsStandbyCompatible(t *testing.T) {
	cfg, err := LoadConfigOptional(t.TempDir(), true)
	if err != nil {
		t.Fatal(err)
	}
	if cfg == nil {
		t.Fatal("optional directory returned a nil config")
	}
}
