package accountbridge

import (
	"crypto/hmac"
	"crypto/rand"
	"crypto/sha256"
	"encoding/base64"
	"encoding/binary"
	"errors"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"runtime"
	"sync"

	"github.com/acosmi/OAuthAPI-LLM/internal/fileperm"
)

const (
	routeSeedSize = 32
	// RouteIDLength is the exact unpadded base64url length of a SHA-256 digest.
	RouteIDLength = 43
	// AccountIDLength is the exact unpadded base64url length of a SHA-256 digest.
	AccountIDLength      = 43
	maxBindingFieldBytes = 4096
)

var (
	// ErrUnknownRoute is returned for malformed, absent, removed, or otherwise
	// unresolved route IDs. Routing must fail loudly and must never fall back to
	// another account.
	ErrUnknownRoute   = errors.New("accountbridge: unknown route")
	ErrRouteCollision = errors.New("accountbridge: route ID collision")
	seedFileMu        sync.Mutex
)

// Binding is the exact internal destination for one opaque route ID. AuthID is
// never encoded into or otherwise recoverable from RouteID.
type Binding struct {
	ConnectorID string
	AuthID      string
	ModelID     string
}

// RouteStore maps stable opaque route IDs to exact account-model bindings.
// The maps are process-local; stability across restarts comes from the
// persistent seed and deterministic registration of current bindings.
type RouteStore struct {
	mu              sync.RWMutex
	seed            [routeSeedSize]byte
	bindings        map[string]Binding
	routesByBinding map[Binding]string
}

// NewRouteStore loads or creates a 32-byte seed at seedPath. Newly created seed
// files use mode 0600. Existing seed files must be regular, exactly 32 bytes,
// and mode 0600 or construction fails closed.
func NewRouteStore(seedPath string) (*RouteStore, error) {
	seed, err := loadOrCreateRouteSeed(seedPath)
	if err != nil {
		return nil, err
	}
	return &RouteStore{
		seed:            seed,
		bindings:        make(map[string]Binding),
		routesByBinding: make(map[Binding]string),
	}, nil
}

// Register adds binding and returns its stable 43-character route ID.
// Registering the same binding repeatedly is idempotent.
func (s *RouteStore) Register(binding Binding) (string, error) {
	if s == nil {
		return "", errors.New("accountbridge: nil route store")
	}
	if err := validateBinding(binding); err != nil {
		return "", err
	}

	s.mu.Lock()
	defer s.mu.Unlock()
	if routeID, ok := s.routesByBinding[binding]; ok {
		return routeID, nil
	}

	routeID := deriveRouteID(s.seed, binding)
	if existing, ok := s.bindings[routeID]; ok {
		if existing == binding {
			s.routesByBinding[binding] = routeID
			return routeID, nil
		}
		return "", ErrRouteCollision
	}
	s.bindings[routeID] = binding
	s.routesByBinding[binding] = routeID
	return routeID, nil
}

// Resolve returns the one exact binding registered for routeID. Unknown and
// malformed route IDs return ErrUnknownRoute; callers must not select a
// substitute account.
func (s *RouteStore) Resolve(routeID string) (Binding, error) {
	if s == nil || !validRouteID(routeID) {
		return Binding{}, ErrUnknownRoute
	}
	s.mu.RLock()
	binding, ok := s.bindings[routeID]
	s.mu.RUnlock()
	if !ok {
		return Binding{}, ErrUnknownRoute
	}
	return binding, nil
}

// Remove invalidates routeID. Removing an unknown route fails loudly.
func (s *RouteStore) Remove(routeID string) error {
	if s == nil || !validRouteID(routeID) {
		return ErrUnknownRoute
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	binding, ok := s.bindings[routeID]
	if !ok {
		return ErrUnknownRoute
	}
	delete(s.bindings, routeID)
	delete(s.routesByBinding, binding)
	return nil
}

// RemoveAccount invalidates every route bound to one exact connector
// credential and reports the number removed. Account removal must make all
// previously issued route IDs fail immediately, even before the model registry
// observes the credential deletion.
func (s *RouteStore) RemoveAccount(connectorID, authID string) int {
	if s == nil || connectorID == "" || authID == "" {
		return 0
	}
	s.mu.Lock()
	defer s.mu.Unlock()
	removed := 0
	for binding, routeID := range s.routesByBinding {
		if binding.ConnectorID != connectorID || binding.AuthID != authID {
			continue
		}
		delete(s.routesByBinding, binding)
		delete(s.bindings, routeID)
		removed++
	}
	return removed
}

// Len reports the number of currently registered exact bindings.
func (s *RouteStore) Len() int {
	if s == nil {
		return 0
	}
	s.mu.RLock()
	defer s.mu.RUnlock()
	return len(s.bindings)
}

func deriveRouteID(seed [routeSeedSize]byte, binding Binding) string {
	mac := hmac.New(sha256.New, seed[:])
	_, _ = mac.Write([]byte("crabcode.accountbridge.route.v1"))
	writeLengthPrefixed(mac, binding.ConnectorID)
	writeLengthPrefixed(mac, binding.AuthID)
	writeLengthPrefixed(mac, binding.ModelID)
	return base64.RawURLEncoding.EncodeToString(mac.Sum(nil))
}

// AccountID returns a stable opaque identifier for one connector credential.
// It is domain-separated from route IDs and cannot reveal the underlying auth ID.
func (s *RouteStore) AccountID(connectorID, authID string) (string, error) {
	if s == nil {
		return "", errors.New("accountbridge: nil route store")
	}
	if connectorID == "" || authID == "" {
		return "", errors.New("accountbridge: empty account binding")
	}
	if len(connectorID) > maxBindingFieldBytes || len(authID) > maxBindingFieldBytes {
		return "", errors.New("accountbridge: account binding field too large")
	}
	mac := hmac.New(sha256.New, s.seed[:])
	_, _ = mac.Write([]byte("crabcode.accountbridge.account.v1"))
	writeLengthPrefixed(mac, connectorID)
	writeLengthPrefixed(mac, authID)
	return base64.RawURLEncoding.EncodeToString(mac.Sum(nil)), nil
}

func writeLengthPrefixed(writer io.Writer, value string) {
	var length [4]byte
	binary.BigEndian.PutUint32(length[:], uint32(len(value)))
	_, _ = writer.Write(length[:])
	_, _ = writer.Write([]byte(value))
}

func validRouteID(routeID string) bool {
	if len(routeID) != RouteIDLength {
		return false
	}
	decoded, err := base64.RawURLEncoding.Strict().DecodeString(routeID)
	return err == nil && len(decoded) == sha256.Size
}

// ValidRouteID reports whether routeID has the canonical opaque route shape.
// It does not reveal whether a route is currently registered.
func ValidRouteID(routeID string) bool {
	return validRouteID(routeID)
}

func validateBinding(binding Binding) error {
	fields := []struct {
		name  string
		value string
	}{
		{name: "connector ID", value: binding.ConnectorID},
		{name: "auth ID", value: binding.AuthID},
		{name: "model ID", value: binding.ModelID},
	}
	for _, field := range fields {
		if len(field.value) == 0 {
			return fmt.Errorf("accountbridge: empty %s", field.name)
		}
		if len(field.value) > maxBindingFieldBytes {
			return fmt.Errorf("accountbridge: %s exceeds %d bytes", field.name, maxBindingFieldBytes)
		}
	}
	return nil
}

func loadOrCreateRouteSeed(seedPath string) ([routeSeedSize]byte, error) {
	var zero [routeSeedSize]byte
	if seedPath == "" {
		return zero, errors.New("accountbridge: empty route seed path")
	}
	if !filepath.IsAbs(seedPath) {
		return zero, errors.New("accountbridge: route seed path must be absolute")
	}

	seedFileMu.Lock()
	defer seedFileMu.Unlock()
	seedDirectory := filepath.Dir(seedPath)
	if _, err := os.Lstat(seedDirectory); err == nil {
		if err := validateRouteSeedDirectory(seedDirectory); err != nil {
			return zero, err
		}
	} else if !errors.Is(err, os.ErrNotExist) {
		return zero, fmt.Errorf("accountbridge: stat route seed directory: %w", err)
	}

	seed, err := readRouteSeed(seedPath)
	if err == nil {
		return seed, nil
	}
	if !errors.Is(err, os.ErrNotExist) {
		return zero, err
	}
	if err := os.MkdirAll(seedDirectory, 0o700); err != nil {
		return zero, fmt.Errorf("accountbridge: create route seed directory: %w", err)
	}
	if err := validateRouteSeedDirectory(seedDirectory); err != nil {
		return zero, err
	}

	if _, err := io.ReadFull(rand.Reader, seed[:]); err != nil {
		return zero, fmt.Errorf("accountbridge: generate route seed: %w", err)
	}
	file, err := os.OpenFile(seedPath, os.O_WRONLY|os.O_CREATE|os.O_EXCL, 0o600)
	if errors.Is(err, os.ErrExist) {
		return readRouteSeed(seedPath)
	}
	if err != nil {
		return zero, fmt.Errorf("accountbridge: create route seed: %w", err)
	}
	written := false
	defer func() {
		if !written {
			_ = os.Remove(seedPath)
		}
	}()
	if err = fileperm.ProtectFile(seedPath); err != nil {
		_ = file.Close()
		return zero, fmt.Errorf("accountbridge: protect route seed before write: %w", err)
	}
	bytesWritten, err := file.Write(seed[:])
	if err != nil {
		_ = file.Close()
		return zero, fmt.Errorf("accountbridge: write route seed: %w", err)
	}
	if bytesWritten != len(seed) {
		_ = file.Close()
		return zero, io.ErrShortWrite
	}
	if err := file.Sync(); err != nil {
		_ = file.Close()
		return zero, fmt.Errorf("accountbridge: sync route seed: %w", err)
	}
	if err := file.Close(); err != nil {
		return zero, fmt.Errorf("accountbridge: close route seed: %w", err)
	}
	written = true
	if runtime.GOOS != "windows" {
		directory, openErr := os.Open(seedDirectory)
		if openErr != nil {
			return zero, fmt.Errorf("accountbridge: open route seed directory for sync: %w", openErr)
		}
		syncErr := directory.Sync()
		closeErr := directory.Close()
		if syncErr != nil {
			return zero, fmt.Errorf("accountbridge: sync route seed directory: %w", syncErr)
		}
		if closeErr != nil {
			return zero, fmt.Errorf("accountbridge: close route seed directory: %w", closeErr)
		}
	}
	return readRouteSeed(seedPath)
}

func validateRouteSeedDirectory(directory string) error {
	if err := fileperm.ProtectDirectory(directory); err != nil {
		return fmt.Errorf("accountbridge: protect route seed directory: %w", err)
	}
	return nil
}

func readRouteSeed(seedPath string) ([routeSeedSize]byte, error) {
	var seed [routeSeedSize]byte
	pathInfo, err := os.Lstat(seedPath)
	if err != nil {
		return seed, err
	}
	if !pathInfo.Mode().IsRegular() {
		return seed, errors.New("accountbridge: route seed must be a regular file")
	}
	if runtime.GOOS != "windows" && pathInfo.Mode().Perm() != 0o600 {
		return seed, fmt.Errorf("accountbridge: route seed mode must be 0600, got %04o", pathInfo.Mode().Perm())
	}
	// Windows mode bits do not encode DACLs. Upgrade an existing seed from an
	// inherited ACL before opening it; Unix retains the prior fail-closed mode
	// check above and only re-applies the already-required 0600 mode.
	if err = fileperm.ProtectFile(seedPath); err != nil {
		return seed, fmt.Errorf("accountbridge: protect route seed: %w", err)
	}
	pathInfo, err = os.Lstat(seedPath)
	if err != nil {
		return seed, fmt.Errorf("accountbridge: restat protected route seed: %w", err)
	}
	file, err := os.Open(seedPath)
	if err != nil {
		return seed, fmt.Errorf("accountbridge: open route seed: %w", err)
	}
	defer file.Close()
	info, err := file.Stat()
	if err != nil {
		return seed, fmt.Errorf("accountbridge: stat route seed: %w", err)
	}
	if !info.Mode().IsRegular() {
		return seed, errors.New("accountbridge: route seed must be a regular file")
	}
	if !os.SameFile(pathInfo, info) {
		return seed, errors.New("accountbridge: route seed changed while opening")
	}
	if runtime.GOOS != "windows" && info.Mode().Perm() != 0o600 {
		return seed, fmt.Errorf("accountbridge: route seed mode must be 0600, got %04o", info.Mode().Perm())
	}
	if err = fileperm.ValidatePrivateFile(seedPath); err != nil {
		return seed, fmt.Errorf("accountbridge: route seed permissions are not private: %w", err)
	}
	if info.Size() != routeSeedSize {
		return seed, fmt.Errorf("accountbridge: route seed must be exactly %d bytes", routeSeedSize)
	}
	data, err := io.ReadAll(io.LimitReader(file, routeSeedSize+1))
	if err != nil {
		return seed, fmt.Errorf("accountbridge: read route seed: %w", err)
	}
	if len(data) != routeSeedSize {
		return seed, fmt.Errorf("accountbridge: route seed must be exactly %d bytes", routeSeedSize)
	}
	copy(seed[:], data)
	return seed, nil
}
