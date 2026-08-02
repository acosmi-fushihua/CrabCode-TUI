package auth

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"runtime"
	"strings"
	"testing"

	"github.com/acosmi/OAuthAPI-LLM/internal/fileperm"
	cliproxyauth "github.com/acosmi/OAuthAPI-LLM/sdk/cliproxy/auth"
)

type encryptedTestTokenStorage struct {
	meta          map[string]any
	savePath      string
	marshalCalled bool
}

func (s *encryptedTestTokenStorage) SetMetadata(meta map[string]any) {
	s.meta = meta
}

func (s *encryptedTestTokenStorage) SaveTokenToFile(path string) error {
	s.savePath = path
	return fmt.Errorf("SaveTokenToFile must not be called by encrypted storage")
}

func (s *encryptedTestTokenStorage) MarshalTokenJSON() ([]byte, error) {
	s.marshalCalled = true
	return json.Marshal(s.meta)
}

func newTestEncryptedStore(t *testing.T, keyByte byte) *FileTokenStore {
	t.Helper()
	store, err := NewEncryptedFileTokenStore(bytes.Repeat([]byte{keyByte}, 32))
	if err != nil {
		t.Fatalf("NewEncryptedFileTokenStore() error = %v", err)
	}
	return store
}

func TestNewEncryptedFileTokenStoreRequiresAES256Key(t *testing.T) {
	if _, err := NewEncryptedFileTokenStore(make([]byte, 31)); err == nil {
		t.Fatal("NewEncryptedFileTokenStore() error = nil, want invalid key length")
	}
	if _, err := NewEncryptedFileTokenStore(make([]byte, 32)); err != nil {
		t.Fatalf("NewEncryptedFileTokenStore() valid key error = %v", err)
	}
}

func TestEncryptedFileTokenStoreStorageRoundTripPermissionsAndDelete(t *testing.T) {
	ctx := context.Background()
	authDir := t.TempDir()
	path := filepath.Join(authDir, "account.json")
	secret := "access-token-must-never-appear-on-disk"
	storage := &encryptedTestTokenStorage{}
	store := newTestEncryptedStore(t, 0x41)
	store.SetBaseDir(authDir)

	auth := &cliproxyauth.Auth{
		ID:       "account.json",
		Provider: "test",
		FileName: "account.json",
		Storage:  storage,
		Metadata: map[string]any{
			"type":         "test",
			"access_token": secret,
			"email":        "masked@example.test",
		},
	}
	savedPath, err := store.Save(ctx, auth)
	if err != nil {
		t.Fatalf("Save() error = %v", err)
	}
	if savedPath != path {
		t.Fatalf("Save() path = %q, want %q", savedPath, path)
	}
	if !storage.marshalCalled {
		t.Fatal("TokenJSONMarshaler.MarshalTokenJSON() was not called")
	}
	if storage.savePath != "" {
		t.Fatalf("plaintext file API was called with %q", storage.savePath)
	}

	raw, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("read encrypted auth file: %v", err)
	}
	if !bytes.HasPrefix(raw, []byte(encryptedFileMagic)) {
		t.Fatalf("encrypted file missing version magic: %x", raw)
	}
	if bytes.Contains(raw, []byte(secret)) || bytes.Contains(raw, []byte("access_token")) {
		t.Fatalf("encrypted file contains token JSON plaintext: %q", raw)
	}
	if err = fileperm.ValidatePrivateFile(path); err != nil {
		t.Fatalf("encrypted auth file is not private: %v", err)
	}
	if runtime.GOOS != "windows" {
		dirInfo, err := os.Stat(authDir)
		if err != nil {
			t.Fatalf("stat auth directory: %v", err)
		}
		if got := dirInfo.Mode().Perm(); got != 0o700 {
			t.Fatalf("encrypted auth directory permissions = %o, want 700", got)
		}
	}
	if err = fileperm.ValidatePrivateDirectory(authDir); err != nil {
		t.Fatalf("encrypted auth directory is not private: %v", err)
	}
	if runtime.GOOS != "windows" {
		if err = os.Chmod(path, 0o644); err != nil {
			t.Fatalf("relax encrypted auth permissions for regression setup: %v", err)
		}
		if err = os.Chmod(authDir, 0o755); err != nil {
			t.Fatalf("relax encrypted auth directory for regression setup: %v", err)
		}
	}
	auth.Storage = nil
	if _, err = store.Save(ctx, auth); err != nil {
		t.Fatalf("unchanged metadata Save() error = %v", err)
	}
	if err = fileperm.ValidatePrivateFile(path); err != nil {
		t.Fatalf("unchanged encrypted auth file is not private: %v", err)
	}
	entries, err := os.ReadDir(authDir)
	if err != nil {
		t.Fatalf("read authDir: %v", err)
	}
	if len(entries) != 1 || entries[0].Name() != "account.json" {
		t.Fatalf("authDir entries = %#v, want only encrypted account.json", entries)
	}
	if runtime.GOOS != "windows" {
		if err = os.Chmod(path, 0o644); err != nil {
			t.Fatalf("relax encrypted auth file before read-only migration: %v", err)
		}
		if err = os.Chmod(authDir, 0o755); err != nil {
			t.Fatalf("relax encrypted auth directory before read-only migration: %v", err)
		}
	}

	auths, err := store.List(ctx)
	if err != nil {
		t.Fatalf("List() error = %v", err)
	}
	if len(auths) != 1 {
		t.Fatalf("List() len = %d, want 1", len(auths))
	}
	if got, _ := auths[0].Metadata["access_token"].(string); got != secret {
		t.Fatalf("decrypted access_token = %q, want original secret", got)
	}
	if err = fileperm.ValidatePrivateDirectory(authDir); err != nil {
		t.Fatalf("read-only list did not protect auth directory: %v", err)
	}
	if err = fileperm.ValidatePrivateFile(path); err != nil {
		t.Fatalf("read-only list did not protect auth file: %v", err)
	}

	if err = store.Delete(ctx, "account.json"); err != nil {
		t.Fatalf("Delete() error = %v", err)
	}
	if _, err = os.Stat(path); !os.IsNotExist(err) {
		t.Fatalf("encrypted auth file exists after Delete(): err=%v", err)
	}
}

func TestEncryptedFileTokenStoreFailsClosed(t *testing.T) {
	ctx := context.Background()

	t.Run("storage without in-memory marshaler is rejected without plaintext output", func(t *testing.T) {
		authDir := t.TempDir()
		path := filepath.Join(authDir, "unsupported.json")
		store := newTestEncryptedStore(t, 0x21)
		store.SetBaseDir(authDir)
		_, err := store.Save(ctx, &cliproxyauth.Auth{
			ID:       "unsupported.json",
			FileName: "unsupported.json",
			Storage:  &testTokenStorage{},
			Metadata: map[string]any{"type": "test", "access_token": "must-not-hit-disk"},
		})
		if err == nil || !strings.Contains(err.Error(), "in-memory JSON serialization") {
			t.Fatalf("Save() error = %v, want in-memory serialization rejection", err)
		}
		if _, statErr := os.Stat(path); !os.IsNotExist(statErr) {
			t.Fatalf("unsupported storage created a file: %v", statErr)
		}
	})

	t.Run("wrong key rejects list and overwrite", func(t *testing.T) {
		authDir := t.TempDir()
		path := filepath.Join(authDir, "account.json")
		writer := newTestEncryptedStore(t, 0x31)
		writer.SetBaseDir(authDir)
		if _, err := writer.Save(ctx, &cliproxyauth.Auth{
			ID:       "account.json",
			FileName: "account.json",
			Metadata: map[string]any{"type": "test", "access_token": "secret-a"},
		}); err != nil {
			t.Fatalf("seed Save() error = %v", err)
		}
		before, err := os.ReadFile(path)
		if err != nil {
			t.Fatalf("read seed file: %v", err)
		}

		wrong := newTestEncryptedStore(t, 0x32)
		wrong.SetBaseDir(authDir)
		if _, err = wrong.List(ctx); err == nil || !strings.Contains(err.Error(), "authentication failed") {
			t.Fatalf("List() wrong-key error = %v, want authentication failure", err)
		}
		if _, err = wrong.Save(ctx, &cliproxyauth.Auth{
			ID:       "account.json",
			FileName: "account.json",
			Metadata: map[string]any{"type": "test", "access_token": "replacement"},
		}); err == nil {
			t.Fatal("Save() with wrong key error = nil")
		}
		after, err := os.ReadFile(path)
		if err != nil {
			t.Fatalf("read file after rejected overwrite: %v", err)
		}
		if !bytes.Equal(after, before) {
			t.Fatal("wrong-key Save() modified the existing encrypted auth file")
		}
	})

	t.Run("tamper rejects list", func(t *testing.T) {
		authDir := t.TempDir()
		path := filepath.Join(authDir, "account.json")
		store := newTestEncryptedStore(t, 0x51)
		store.SetBaseDir(authDir)
		if _, err := store.Save(ctx, &cliproxyauth.Auth{
			ID:       "account.json",
			FileName: "account.json",
			Metadata: map[string]any{"type": "test", "access_token": "secret-b"},
		}); err != nil {
			t.Fatalf("Save() error = %v", err)
		}
		raw, err := os.ReadFile(path)
		if err != nil {
			t.Fatalf("read encrypted file: %v", err)
		}
		raw[len(raw)-1] ^= 0x80
		if err = os.WriteFile(path, raw, 0o600); err != nil {
			t.Fatalf("write tampered file: %v", err)
		}
		if _, err = store.List(ctx); err == nil || !strings.Contains(err.Error(), "authentication failed") {
			t.Fatalf("List() tamper error = %v, want authentication failure", err)
		}
	})

	t.Run("plaintext legacy file rejects list", func(t *testing.T) {
		authDir := t.TempDir()
		path := filepath.Join(authDir, "legacy.json")
		plaintext := []byte(`{"type":"test","access_token":"legacy-secret"}`)
		if err := os.WriteFile(path, plaintext, 0o600); err != nil {
			t.Fatalf("write legacy plaintext: %v", err)
		}
		store := newTestEncryptedStore(t, 0x61)
		store.SetBaseDir(authDir)
		if _, err := store.List(ctx); err == nil || !strings.Contains(err.Error(), "invalid or unsupported format") {
			t.Fatalf("List() plaintext error = %v, want format rejection", err)
		}
	})
}

func TestEncryptedFileTokenStoreUsesFreshNonceAndKeepsMetadataRewriteEncrypted(t *testing.T) {
	authDir := t.TempDir()
	path := filepath.Join(authDir, "antigravity.json")
	store := newTestEncryptedStore(t, 0x71)
	store.SetBaseDir(authDir)
	firstPlaintext := []byte(`{"type":"antigravity","access_token":"secret-c","project_id":"old"}`)
	if err := store.writeAuthPayload(path, firstPlaintext); err != nil {
		t.Fatalf("first writeAuthPayload() error = %v", err)
	}
	firstCiphertext, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("read first ciphertext: %v", err)
	}
	if err = store.writeAuthPayload(path, firstPlaintext); err != nil {
		t.Fatalf("second writeAuthPayload() error = %v", err)
	}
	secondCiphertext, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("read second ciphertext: %v", err)
	}
	if bytes.Equal(firstCiphertext, secondCiphertext) {
		t.Fatal("same plaintext produced identical ciphertext; nonce was not fresh")
	}

	updated := []byte(`{"type":"antigravity","access_token":"secret-c","project_id":"new"}`)
	if err = store.writeAuthPayload(path, updated); err != nil {
		t.Fatalf("metadata rewrite error = %v", err)
	}
	raw, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("read metadata rewrite: %v", err)
	}
	if !bytes.HasPrefix(raw, []byte(encryptedFileMagic)) || bytes.Contains(raw, []byte("secret-c")) {
		t.Fatalf("metadata rewrite was not encrypted: %q", raw)
	}
	auths, err := store.List(context.Background())
	if err != nil {
		t.Fatalf("List() after metadata rewrite error = %v", err)
	}
	if len(auths) != 1 || auths[0].Metadata["project_id"] != "new" {
		t.Fatalf("decrypted metadata after rewrite = %#v", auths)
	}
}

func TestNewFileTokenStoreRemainsPlaintextCompatible(t *testing.T) {
	authDir := t.TempDir()
	path := filepath.Join(authDir, "plain.json")
	store := NewFileTokenStore()
	store.SetBaseDir(authDir)
	secret := "upstream-plaintext-compatibility"
	if _, err := store.Save(context.Background(), &cliproxyauth.Auth{
		ID:       "plain.json",
		FileName: "plain.json",
		Metadata: map[string]any{"type": "test", "access_token": secret},
	}); err != nil {
		t.Fatalf("Save() error = %v", err)
	}
	raw, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("read plaintext auth file: %v", err)
	}
	if !json.Valid(raw) || !bytes.Contains(raw, []byte(secret)) {
		t.Fatalf("NewFileTokenStore no longer writes plaintext JSON: %q", raw)
	}
}
