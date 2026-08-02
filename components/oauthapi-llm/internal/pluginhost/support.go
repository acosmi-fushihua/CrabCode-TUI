package pluginhost

// SupportPluginHeaderValue reports whether native dynamic-library plugin loading
// is available in the current binary.
func SupportPluginHeaderValue() string {
	return supportPluginValue
}
