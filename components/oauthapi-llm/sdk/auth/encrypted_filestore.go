package auth

import (
	"bytes"
	"crypto/aes"
	"crypto/cipher"
	"crypto/rand"
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"runtime"

	baseauth "github.com/acosmi/OAuthAPI-LLM/internal/auth"
	"github.com/acosmi/OAuthAPI-LLM/internal/fileperm"
)

const encryptedFileMagic = "CRABAUTH\x00\x01"

type encryptedFileCodec struct {
	aead cipher.AEAD
}

// NewEncryptedFileTokenStore creates the production filesystem store. The
// caller must provide exactly 32 bytes of key material for AES-256-GCM.
func NewEncryptedFileTokenStore(masterKey []byte) (*FileTokenStore, error) {
	if len(masterKey) != 32 {
		return nil, fmt.Errorf("auth filestore: AES-256 master key must be exactly 32 bytes")
	}
	block, err := aes.NewCipher(masterKey)
	if err != nil {
		return nil, fmt.Errorf("auth filestore: initialize AES-256: %w", err)
	}
	aead, err := cipher.NewGCM(block)
	if err != nil {
		return nil, fmt.Errorf("auth filestore: initialize AES-GCM: %w", err)
	}
	return &FileTokenStore{encrypted: &encryptedFileCodec{aead: aead}}, nil
}

func (c *encryptedFileCodec) seal(plaintext []byte) ([]byte, error) {
	nonce := make([]byte, c.aead.NonceSize())
	if _, err := rand.Read(nonce); err != nil {
		return nil, fmt.Errorf("generate nonce: %w", err)
	}
	magic := []byte(encryptedFileMagic)
	out := make([]byte, 0, len(magic)+len(nonce)+len(plaintext)+c.aead.Overhead())
	out = append(out, magic...)
	out = append(out, nonce...)
	out = c.aead.Seal(out, nonce, plaintext, magic)
	return out, nil
}

func (c *encryptedFileCodec) open(payload []byte) ([]byte, error) {
	magic := []byte(encryptedFileMagic)
	minimumSize := len(magic) + c.aead.NonceSize() + c.aead.Overhead()
	if len(payload) < minimumSize || !bytes.HasPrefix(payload, magic) {
		return nil, fmt.Errorf("encrypted auth file has an invalid or unsupported format")
	}
	nonceStart := len(magic)
	nonceEnd := nonceStart + c.aead.NonceSize()
	plaintext, err := c.aead.Open(nil, payload[nonceStart:nonceEnd], payload[nonceEnd:], magic)
	if err != nil {
		return nil, fmt.Errorf("encrypted auth file authentication failed")
	}
	return plaintext, nil
}

func (s *FileTokenStore) isEncrypted() bool {
	return s != nil && s.encrypted != nil
}

func (s *FileTokenStore) readAuthPayload(path string) ([]byte, error) {
	if s.isEncrypted() {
		if err := fileperm.ProtectFile(path); err != nil {
			return nil, err
		}
	}
	payload, err := os.ReadFile(path)
	if err != nil {
		return nil, err
	}
	if !s.isEncrypted() {
		return payload, nil
	}
	plaintext, err := s.encrypted.open(payload)
	if err != nil {
		return nil, fmt.Errorf("auth filestore: decrypt %s: %w", filepath.Base(path), err)
	}
	return plaintext, nil
}

func (s *FileTokenStore) writeAuthPayload(path string, plaintext []byte) error {
	payload := plaintext
	if s.isEncrypted() {
		if err := ensurePrivateAuthDirectory(filepath.Dir(path)); err != nil {
			return fmt.Errorf("auth filestore: protect auth directory: %w", err)
		}
		var err error
		payload, err = s.encrypted.seal(plaintext)
		if err != nil {
			return fmt.Errorf("auth filestore: encrypt %s: %w", filepath.Base(path), err)
		}
	}
	if err := atomicWriteFile0600(path, payload); err != nil {
		return fmt.Errorf("auth filestore: persist %s: %w", filepath.Base(path), err)
	}
	return nil
}

func ensurePrivateAuthDirectory(dir string) error {
	if err := os.MkdirAll(dir, 0o700); err != nil {
		return err
	}
	return fileperm.ProtectDirectory(dir)
}

func (s *FileTokenStore) validateEncryptedDestination(path string) error {
	if !s.isEncrypted() {
		return nil
	}
	if _, err := s.readAuthPayload(path); err != nil {
		if os.IsNotExist(err) {
			return nil
		}
		return err
	}
	return nil
}

func captureTokenStorageJSON(storage baseauth.TokenStorage) ([]byte, error) {
	marshaler, ok := storage.(baseauth.TokenJSONMarshaler)
	if !ok {
		return nil, fmt.Errorf("auth filestore: token storage does not support in-memory JSON serialization")
	}
	raw, err := marshaler.MarshalTokenJSON()
	if err != nil {
		return nil, fmt.Errorf("auth filestore: serialize token in memory: %w", err)
	}
	if !json.Valid(raw) {
		return nil, fmt.Errorf("auth filestore: token storage produced invalid JSON")
	}
	return bytes.Clone(raw), nil
}

func atomicWriteFile0600(path string, data []byte) error {
	dir := filepath.Dir(path)
	if err := os.MkdirAll(dir, 0o700); err != nil {
		return fmt.Errorf("create directory: %w", err)
	}
	file, err := os.CreateTemp(dir, "."+filepath.Base(path)+".tmp-*")
	if err != nil {
		return fmt.Errorf("create encrypted temporary file: %w", err)
	}
	tempPath := file.Name()
	closed := false
	defer func() {
		if !closed {
			_ = file.Close()
		}
		_ = os.Remove(tempPath)
	}()

	if err = file.Chmod(0o600); err != nil {
		return fmt.Errorf("chmod encrypted temporary file: %w", err)
	}
	if err = fileperm.ProtectFile(tempPath); err != nil {
		return fmt.Errorf("protect encrypted temporary file: %w", err)
	}
	if _, err = file.Write(data); err != nil {
		return fmt.Errorf("write encrypted temporary file: %w", err)
	}
	if err = file.Sync(); err != nil {
		return fmt.Errorf("sync encrypted temporary file: %w", err)
	}
	if err = file.Close(); err != nil {
		return fmt.Errorf("close encrypted temporary file: %w", err)
	}
	closed = true
	if err = os.Rename(tempPath, path); err != nil {
		return fmt.Errorf("replace auth file: %w", err)
	}
	tempPath = ""
	if err = fileperm.ProtectFile(path); err != nil {
		return fmt.Errorf("protect auth file: %w", err)
	}
	// Persist the directory entry as well as the ciphertext. Windows does not
	// support fsync on directory handles, while Unix release targets do.
	if runtime.GOOS != "windows" {
		directory, openErr := os.Open(dir)
		if openErr != nil {
			return fmt.Errorf("open auth directory for sync: %w", openErr)
		}
		syncErr := directory.Sync()
		closeErr := directory.Close()
		if syncErr != nil {
			return fmt.Errorf("sync auth directory: %w", syncErr)
		}
		if closeErr != nil {
			return fmt.Errorf("close auth directory after sync: %w", closeErr)
		}
	}
	return nil
}
