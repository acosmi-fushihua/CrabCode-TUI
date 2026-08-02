// Package auth provides authentication functionality for various AI service providers.
// It includes interfaces and implementations for token storage and authentication methods.
package auth

// TokenStorage defines the interface for storing authentication tokens.
// Implementations of this interface should provide methods to persist
// authentication tokens to a file system location.
type TokenStorage interface {
	// SaveTokenToFile persists authentication tokens to the specified file path.
	//
	// Parameters:
	//   - authFilePath: The file path where the authentication tokens should be saved
	//
	// Returns:
	//   - error: An error if the save operation fails, nil otherwise
	SaveTokenToFile(authFilePath string) error
}

// TokenJSONMarshaler is the in-memory persistence boundary required by the
// encrypted Account Bridge store. Implementations must return the exact JSON
// payload they would otherwise write, without creating a plaintext file.
// The production encrypted store rejects TokenStorage values that do not
// implement this interface instead of falling back to disk staging.
type TokenJSONMarshaler interface {
	MarshalTokenJSON() ([]byte, error)
}
