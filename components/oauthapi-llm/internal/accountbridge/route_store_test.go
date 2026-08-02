package accountbridge

import (
	"encoding/base64"
	"errors"
	"os"
	"path/filepath"
	"runtime"
	"strings"
	"sync"
	"testing"

	"github.com/acosmi/OAuthAPI-LLM/internal/fileperm"
)

func TestRouteStoreCreatesPersistentPrivateSeedAndStableRouteID(t *testing.T) {
	seedPath := filepath.Join(t.TempDir(), "nested", "route.seed")
	binding := Binding{
		ConnectorID: "openai",
		AuthID:      "provider-subject:alice@example.invalid",
		ModelID:     "model/private-account-route",
	}

	store, err := NewRouteStore(seedPath)
	if err != nil {
		t.Fatal(err)
	}
	routeID, err := store.Register(binding)
	if err != nil {
		t.Fatal(err)
	}
	if len(routeID) != RouteIDLength {
		t.Fatalf("route ID length = %d, want %d", len(routeID), RouteIDLength)
	}
	decoded, err := base64.RawURLEncoding.Strict().DecodeString(routeID)
	if err != nil || len(decoded) != 32 {
		t.Fatalf("route ID is not a full unpadded SHA-256 base64url value: %q, %v", routeID, err)
	}
	if strings.Contains(routeID, "=") {
		t.Fatalf("route ID must be unpadded: %q", routeID)
	}
	for _, secretFragment := range []string{binding.AuthID, "alice@example.invalid", binding.ModelID} {
		if strings.Contains(routeID, secretFragment) {
			t.Fatalf("route ID leaked binding material %q", secretFragment)
		}
	}

	info, err := os.Stat(seedPath)
	if err != nil {
		t.Fatal(err)
	}
	if runtime.GOOS != "windows" && info.Mode().Perm() != 0o600 {
		t.Fatalf("seed mode = %04o, want 0600", info.Mode().Perm())
	}
	if err = fileperm.ValidatePrivateFile(seedPath); err != nil {
		t.Fatalf("route seed is not private: %v", err)
	}
	if info.Size() != routeSeedSize {
		t.Fatalf("seed size = %d, want %d", info.Size(), routeSeedSize)
	}
	if runtime.GOOS != "windows" {
		dirInfo, err := os.Stat(filepath.Dir(seedPath))
		if err != nil {
			t.Fatal(err)
		}
		if got := dirInfo.Mode().Perm(); got != 0o700 {
			t.Fatalf("seed directory mode = %04o, want 0700", got)
		}
	}
	if err = fileperm.ValidatePrivateDirectory(filepath.Dir(seedPath)); err != nil {
		t.Fatalf("route seed directory is not private: %v", err)
	}

	reopened, err := NewRouteStore(seedPath)
	if err != nil {
		t.Fatal(err)
	}
	reopenedRouteID, err := reopened.Register(binding)
	if err != nil {
		t.Fatal(err)
	}
	if reopenedRouteID != routeID {
		t.Fatalf("route ID changed across restart: %q != %q", reopenedRouteID, routeID)
	}
}

func TestRouteStorePinnedHMACBase64URLVector(t *testing.T) {
	seedPath := filepath.Join(t.TempDir(), "route.seed")
	seed := make([]byte, routeSeedSize)
	for index := range seed {
		seed[index] = byte(index)
	}
	if err := os.WriteFile(seedPath, seed, 0o600); err != nil {
		t.Fatal(err)
	}
	store, err := NewRouteStore(seedPath)
	if err != nil {
		t.Fatal(err)
	}
	routeID, err := store.Register(Binding{ConnectorID: "openai", AuthID: "auth-123", ModelID: "gpt-5"})
	if err != nil {
		t.Fatal(err)
	}
	const expected = "nCeL2LChKtCGIGtnh4sJrdEN38yvbYqniP9GD4IbtIs"
	if routeID != expected {
		t.Fatalf("route contract drifted: got %q, want %q", routeID, expected)
	}
}

func TestRouteStoreExactBindingIdempotencyAndFailLoud(t *testing.T) {
	store, err := NewRouteStore(filepath.Join(t.TempDir(), "route.seed"))
	if err != nil {
		t.Fatal(err)
	}
	binding := Binding{ConnectorID: "anthropic", AuthID: "auth-1", ModelID: "claude-model"}
	routeID, err := store.Register(binding)
	if err != nil {
		t.Fatal(err)
	}
	duplicate, err := store.Register(binding)
	if err != nil {
		t.Fatal(err)
	}
	if duplicate != routeID || store.Len() != 1 {
		t.Fatalf("duplicate registration was not idempotent: %q, %q, len=%d", routeID, duplicate, store.Len())
	}

	resolved, err := store.Resolve(routeID)
	if err != nil || resolved != binding {
		t.Fatalf("resolved binding = %+v, %v", resolved, err)
	}
	for _, changed := range []Binding{
		{ConnectorID: "xai", AuthID: binding.AuthID, ModelID: binding.ModelID},
		{ConnectorID: binding.ConnectorID, AuthID: "auth-2", ModelID: binding.ModelID},
		{ConnectorID: binding.ConnectorID, AuthID: binding.AuthID, ModelID: "another-model"},
	} {
		changedRoute, err := store.Register(changed)
		if err != nil {
			t.Fatal(err)
		}
		if changedRoute == routeID {
			t.Fatalf("distinct exact binding reused route ID: %+v", changed)
		}
	}

	unknownID := strings.Repeat("A", RouteIDLength)
	if _, err := store.Resolve(unknownID); !errors.Is(err, ErrUnknownRoute) {
		t.Fatalf("unknown route must fail loudly, got %v", err)
	}
	if _, err := store.Resolve("not-base64url"); !errors.Is(err, ErrUnknownRoute) {
		t.Fatalf("malformed route must fail loudly, got %v", err)
	}
	if err := store.Remove(routeID); err != nil {
		t.Fatal(err)
	}
	if _, err := store.Resolve(routeID); !errors.Is(err, ErrUnknownRoute) {
		t.Fatalf("removed route must fail loudly, got %v", err)
	}
	if err := store.Remove(routeID); !errors.Is(err, ErrUnknownRoute) {
		t.Fatalf("duplicate removal must fail loudly, got %v", err)
	}
}

func TestRouteStoreConcurrentRegistrationAndResolution(t *testing.T) {
	store, err := NewRouteStore(filepath.Join(t.TempDir(), "route.seed"))
	if err != nil {
		t.Fatal(err)
	}
	binding := Binding{ConnectorID: "google", AuthID: "auth-concurrent", ModelID: "gemini-model"}

	const workers = 64
	routeIDs := make(chan string, workers)
	errorsCh := make(chan error, workers)
	var waitGroup sync.WaitGroup
	for range workers {
		waitGroup.Add(1)
		go func() {
			defer waitGroup.Done()
			routeID, err := store.Register(binding)
			if err != nil {
				errorsCh <- err
				return
			}
			resolved, err := store.Resolve(routeID)
			if err != nil {
				errorsCh <- err
				return
			}
			if resolved != binding {
				errorsCh <- errors.New("resolved binding mismatch")
				return
			}
			routeIDs <- routeID
		}()
	}
	waitGroup.Wait()
	close(errorsCh)
	close(routeIDs)
	for err := range errorsCh {
		t.Fatal(err)
	}
	var first string
	for routeID := range routeIDs {
		if first == "" {
			first = routeID
		}
		if routeID != first {
			t.Fatalf("concurrent idempotent registration returned %q and %q", first, routeID)
		}
	}
	if store.Len() != 1 {
		t.Fatalf("store len = %d, want 1", store.Len())
	}
}

func TestRouteStoreRemoveAccountInvalidatesOnlyExactAccountRoutes(t *testing.T) {
	store, err := NewRouteStore(filepath.Join(t.TempDir(), "route.seed"))
	if err != nil {
		t.Fatal(err)
	}
	targetA, err := store.Register(Binding{ConnectorID: "openai", AuthID: "auth-1", ModelID: "model-a"})
	if err != nil {
		t.Fatal(err)
	}
	targetB, err := store.Register(Binding{ConnectorID: "openai", AuthID: "auth-1", ModelID: "model-b"})
	if err != nil {
		t.Fatal(err)
	}
	otherConnector, err := store.Register(Binding{ConnectorID: "anthropic", AuthID: "auth-1", ModelID: "model-a"})
	if err != nil {
		t.Fatal(err)
	}
	otherAccount, err := store.Register(Binding{ConnectorID: "openai", AuthID: "auth-2", ModelID: "model-a"})
	if err != nil {
		t.Fatal(err)
	}

	if removed := store.RemoveAccount("openai", "auth-1"); removed != 2 {
		t.Fatalf("removed = %d, want 2", removed)
	}
	for _, routeID := range []string{targetA, targetB} {
		if _, err := store.Resolve(routeID); !errors.Is(err, ErrUnknownRoute) {
			t.Fatalf("removed account route %q remained resolvable: %v", routeID, err)
		}
	}
	for _, routeID := range []string{otherConnector, otherAccount} {
		if _, err := store.Resolve(routeID); err != nil {
			t.Fatalf("unrelated route %q was removed: %v", routeID, err)
		}
	}
	if removed := store.RemoveAccount("openai", "auth-1"); removed != 0 {
		t.Fatalf("second removal = %d, want 0", removed)
	}
}

func TestRouteStoreRejectsUnsafeSeedAndBinding(t *testing.T) {
	t.Run("relative seed path", func(t *testing.T) {
		if _, err := NewRouteStore("route.seed"); err == nil {
			t.Fatal("expected relative seed path to be rejected")
		}
	})

	t.Run("wrong seed size", func(t *testing.T) {
		path := filepath.Join(t.TempDir(), "route.seed")
		if err := os.WriteFile(path, []byte("short"), 0o600); err != nil {
			t.Fatal(err)
		}
		if _, err := NewRouteStore(path); err == nil {
			t.Fatal("expected short seed to be rejected")
		}
	})

	t.Run("wide seed permissions", func(t *testing.T) {
		if runtime.GOOS == "windows" {
			// POSIX permission bits are advisory on Windows and the store's
			// production check deliberately exempts the platform, matching the
			// other permission assertions in this file.
			t.Skip("POSIX seed permissions are not enforced on windows")
		}
		path := filepath.Join(t.TempDir(), "route.seed")
		if err := os.WriteFile(path, make([]byte, routeSeedSize), 0o644); err != nil {
			t.Fatal(err)
		}
		if err := os.Chmod(path, 0o644); err != nil {
			t.Fatal(err)
		}
		if _, err := NewRouteStore(path); err == nil {
			t.Fatal("expected wide seed permissions to be rejected")
		}
	})

	t.Run("symlink seed", func(t *testing.T) {
		directory := t.TempDir()
		target := filepath.Join(directory, "target.seed")
		path := filepath.Join(directory, "route.seed")
		if err := os.WriteFile(target, make([]byte, routeSeedSize), 0o600); err != nil {
			t.Fatal(err)
		}
		if err := os.Symlink(target, path); err != nil {
			t.Skipf("symlink is not available on this platform: %v", err)
		}
		if _, err := NewRouteStore(path); err == nil {
			t.Fatal("expected symlink seed to be rejected")
		}
	})

	store, err := NewRouteStore(filepath.Join(t.TempDir(), "route.seed"))
	if err != nil {
		t.Fatal(err)
	}
	for _, binding := range []Binding{
		{AuthID: "auth", ModelID: "model"},
		{ConnectorID: "connector", ModelID: "model"},
		{ConnectorID: "connector", AuthID: "auth"},
	} {
		if _, err := store.Register(binding); err == nil {
			t.Fatalf("expected invalid binding to be rejected: %+v", binding)
		}
	}
}
