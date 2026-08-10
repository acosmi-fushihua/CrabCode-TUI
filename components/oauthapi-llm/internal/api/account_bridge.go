package api

import (
	"bytes"
	"context"
	"io"
	"math"
	"net/http"
	"strings"
	"sync"
	"time"

	"github.com/acosmi/OAuthAPI-LLM/internal/accountbridge"
	accountbridgequota "github.com/acosmi/OAuthAPI-LLM/internal/accountbridge/quota"
	managementHandlers "github.com/acosmi/OAuthAPI-LLM/internal/api/handlers/management"
	"github.com/acosmi/OAuthAPI-LLM/internal/registry"
	"github.com/acosmi/OAuthAPI-LLM/sdk/api/handlers"
	sdkauth "github.com/acosmi/OAuthAPI-LLM/sdk/auth"
	coreauth "github.com/acosmi/OAuthAPI-LLM/sdk/cliproxy/auth"
	"github.com/gin-gonic/gin"
	"github.com/tidwall/gjson"
)

const (
	accountRouteHeader       = "X-Account-Route-Id"
	maxAccountBridgeBodySize = 32 << 20
	maxUsageRouteFilters     = 1024
	maxQuotaFetchConcurrency = 8
	accountBridgeQuotaBudget = 12 * time.Second
)

type facadeConnector struct {
	ConnectorID      string  `json:"connectorId"`
	SourceProviderID string  `json:"sourceProviderId"`
	DisplayName      string  `json:"displayName"`
	AuthMode         string  `json:"authMode"`
	Enabled          bool    `json:"enabled"`
	DisabledReason   *string `json:"disabledReasonCode"`
	TermsStatus      string  `json:"termsStatus"`
}

type facadeAccount struct {
	AccountID     string  `json:"accountId"`
	ConnectorID   string  `json:"connectorId"`
	DisplayLabel  string  `json:"displayLabel"`
	Status        string  `json:"status"`
	ConnectedAt   string  `json:"connectedAt"`
	LastUsedAt    *string `json:"lastUsedAt"`
	CooldownUntil *string `json:"cooldownUntil"`
}

type facadeModel struct {
	RouteID                  string   `json:"routeId"`
	AccountID                string   `json:"accountId"`
	ConnectorID              string   `json:"connectorId"`
	ModelID                  string   `json:"modelId"`
	DisplayName              *string  `json:"displayName"`
	ConnectorLabel           string   `json:"connectorLabel"`
	AccountLabel             string   `json:"accountLabel"`
	ChatRuntimeSupported     *bool    `json:"chatRuntimeSupported"`
	SupportsTools            *bool    `json:"supportsTools"`
	SupportsThinking         *bool    `json:"supportsThinking"`
	SupportsAdaptiveThinking *bool    `json:"supportsAdaptiveThinking"`
	SupportsEffort           *bool    `json:"supportsEffort"`
	SupportsMaxEffort        *bool    `json:"supportsMaxEffort"`
	SupportsVision           *bool    `json:"supportsVision"`
	SupportsJSONMode         *bool    `json:"supportsJsonMode"`
	SupportedThinkingModes   []string `json:"supportedThinkingModes"`
	DefaultThinkingMode      *string  `json:"defaultThinkingMode"`
	ContextWindow            *uint64  `json:"contextWindow"`
	MaxOutputTokens          *uint64  `json:"maxOutputTokens"`
}

type facadeUsageWindow struct {
	Label            string   `json:"label"`
	Limit            *uint64  `json:"limit"`
	Used             *uint64  `json:"used"`
	RemainingPercent *float64 `json:"remainingPercent"`
	ResetsAt         *string  `json:"resetsAt"`
}

type facadeUsage struct {
	RouteID             string              `json:"routeId"`
	AccountID           string              `json:"accountId"`
	State               string              `json:"state"`
	RemainingPercent    *float64            `json:"remainingPercent"`
	LimitingWindowLabel *string             `json:"limitingWindowLabel"`
	ResetsAt            *string             `json:"resetsAt"`
	Windows             []facadeUsageWindow `json:"windows"`
	ObservedAt          string              `json:"observedAt"`
}

type accountBridgeQuotaReader interface {
	Read(context.Context, *coreauth.Auth, string, bool) (accountbridgequota.Report, error)
}

func (s *Server) accountBridgeEligibilityMiddleware() gin.HandlerFunc {
	return func(c *gin.Context) {
		// Boot-level gate only: per-connector eligibility (membership, policy
		// gates, CN floor) is enforced by the individual handlers.
		if s == nil || s.accountBridgeRuntime == nil || !s.accountBridgeRuntime.Decision().BootAllowed() {
			c.AbortWithStatusJSON(http.StatusForbidden, gin.H{"error": "eligibility_denied"})
			return
		}
		c.Next()
	}
}

func (s *Server) registerAccountBridgeFacade(group *gin.RouterGroup) {
	group.GET("/connectors", s.accountBridgeConnectors)
	group.GET("/accounts", s.accountBridgeAccounts)
	group.DELETE("/accounts/:accountId", s.accountBridgeAccountRemove)
	group.GET("/models", s.accountBridgeModels)
	group.GET("/routes", s.accountBridgeRoutes)
	group.GET("/usage", s.accountBridgeUsage)
	group.POST("/login/start", s.accountBridgeLoginStart)
	group.GET("/login/anthropic/start", s.accountBridgeLoginStartFor(accountbridge.ConnectorAnthropic))
	group.GET("/login/openai/start", s.accountBridgeLoginStartFor(accountbridge.ConnectorOpenAI))
	group.GET("/login/google/start", s.accountBridgeLoginStartFor(accountbridge.ConnectorGoogle))
	group.GET("/login/xai/start", s.accountBridgeLoginStartFor(accountbridge.ConnectorXAI))
	group.GET("/login/qwen/start", s.accountBridgeLoginStartFor(accountbridge.ConnectorQwen))
	group.GET("/login/kimi/start", s.accountBridgeLoginStartFor(accountbridge.ConnectorKimi))
	group.GET("/login/zai/start", s.accountBridgeLoginStartFor(accountbridge.ConnectorZai))
	group.GET("/login/poll", s.accountBridgeLoginPoll)
	group.DELETE("/login/cancel", s.accountBridgeLoginCancel)
}

func (s *Server) accountBridgeConnectors(c *gin.Context) {
	items := make([]facadeConnector, 0, 4)
	for _, connectorID := range accountbridge.ConnectorIDs() {
		sourceProviderID, _ := accountbridge.ProviderForConnector(connectorID)
		enabled := s != nil && s.accountBridgeRuntime != nil && s.accountBridgeRuntime.ConnectorEnabled(connectorID)
		disabledReason := ""
		termsStatus := "blocked"
		displayName := ""
		authMode := ""
		if s != nil && s.accountBridgeRuntime != nil {
			disabledReason = s.accountBridgeRuntime.ConnectorDisabledReason(connectorID)
			termsStatus = s.accountBridgeRuntime.ConnectorTermsStatus(connectorID)
			displayName = s.accountBridgeRuntime.ConnectorDisplayName(connectorID)
			authMode = s.accountBridgeRuntime.ConnectorAuthMode(connectorID)
		}
		var disabledReasonPtr *string
		if disabledReason != "" {
			disabledReasonCopy := disabledReason
			disabledReasonPtr = &disabledReasonCopy
		}
		items = append(items, facadeConnector{
			ConnectorID:      connectorID,
			SourceProviderID: sourceProviderID,
			DisplayName:      displayName,
			AuthMode:         authMode,
			Enabled:          enabled,
			DisabledReason:   disabledReasonPtr,
			TermsStatus:      termsStatus,
		})
	}
	c.JSON(http.StatusOK, gin.H{"connectors": items})
}

func (s *Server) accountBridgeAccounts(c *gin.Context) {
	accounts, _, _ := s.accountBridgeSnapshot()
	c.JSON(http.StatusOK, gin.H{"accounts": accounts})
}

func (s *Server) accountBridgeModels(c *gin.Context) {
	_, models, _ := s.accountBridgeSnapshot()
	c.JSON(http.StatusOK, gin.H{"models": models})
}

func (s *Server) accountBridgeRoutes(c *gin.Context) {
	_, models, _ := s.accountBridgeSnapshot()
	c.JSON(http.StatusOK, gin.H{"routes": models})
}

func (s *Server) accountBridgeUsage(c *gin.Context) {
	forceRefreshValues, hasForceRefresh := c.Request.URL.Query()["forceRefresh"]
	if hasForceRefresh && (len(forceRefreshValues) != 1 || (forceRefreshValues[0] != "true" && forceRefreshValues[0] != "false")) {
		c.JSON(http.StatusBadRequest, gin.H{"error": "invalid_force_refresh"})
		return
	}
	_, _, usage := s.accountBridgeSnapshot()
	requestedRouteIDs := c.QueryArray("routeId")
	if len(requestedRouteIDs) > maxUsageRouteFilters {
		c.JSON(http.StatusBadRequest, gin.H{"error": "too_many_routes"})
		return
	}
	if len(requestedRouteIDs) > 0 {
		requested := make(map[string]struct{}, len(requestedRouteIDs))
		for _, routeID := range requestedRouteIDs {
			routeID = strings.TrimSpace(routeID)
			if !accountbridge.ValidRouteID(routeID) {
				c.JSON(http.StatusBadRequest, gin.H{"error": "invalid_route"})
				return
			}
			requested[routeID] = struct{}{}
		}
		filtered := make([]facadeUsage, 0, len(requested))
		for _, snapshot := range usage {
			if _, ok := requested[snapshot.RouteID]; ok {
				filtered = append(filtered, snapshot)
			}
		}
		usage = filtered
	}
	forceRefresh := hasForceRefresh && forceRefreshValues[0] == "true"
	usage = s.enrichAccountBridgeUsage(c.Request.Context(), usage, forceRefresh)
	c.JSON(http.StatusOK, gin.H{"snapshots": usage})
}

type accountBridgeQuotaReadResult struct {
	report accountbridgequota.Report
	ok     bool
}

type accountBridgeQuotaTarget struct {
	credential  *coreauth.Auth
	connectorID string
	result      accountBridgeQuotaReadResult
}

type accountBridgeUsageBinding struct {
	targetKey string
	modelID   string
	valid     bool
}

func (s *Server) enrichAccountBridgeUsage(ctx context.Context, usage []facadeUsage, forceRefresh bool) []facadeUsage {
	if s == nil || s.accountBridgeRuntime == nil || s.accountBridgeQuota == nil || len(usage) == 0 {
		return usage
	}
	routes := s.accountBridgeRuntime.Routes()
	manager := s.accountBridgeRuntime.AuthManager()
	if routes == nil || manager == nil {
		return usage
	}
	targets := make(map[string]*accountBridgeQuotaTarget)
	bindings := make([]accountBridgeUsageBinding, len(usage))
	for index := range usage {
		binding, err := routes.Resolve(usage[index].RouteID)
		if err != nil {
			continue
		}
		credential, ok := manager.GetByID(binding.AuthID)
		if !ok || credential == nil {
			continue
		}
		targetKey := binding.ConnectorID + "\x00" + binding.AuthID
		bindings[index] = accountBridgeUsageBinding{targetKey: targetKey, modelID: binding.ModelID, valid: true}
		if _, exists := targets[targetKey]; !exists {
			targets[targetKey] = &accountBridgeQuotaTarget{credential: credential, connectorID: binding.ConnectorID}
		}
	}
	if len(targets) == 0 {
		return usage
	}

	quotaContext, cancel := context.WithTimeout(ctx, accountBridgeQuotaBudget)
	defer cancel()
	targetChannel := make(chan *accountBridgeQuotaTarget)
	var waitGroup sync.WaitGroup
	for range min(maxQuotaFetchConcurrency, len(targets)) {
		waitGroup.Add(1)
		go func() {
			defer waitGroup.Done()
			// F4: a panic in one quota probe must cost that probe, not the
			// sidecar. Registered after Done so it runs first and stops the
			// unwind before it crosses the goroutine boundary.
			defer managementHandlers.RecoverAccountBridgeGoroutine("quota fan-out worker")
			for target := range targetChannel {
				if quotaContext.Err() != nil {
					return
				}
				report, readErr := s.accountBridgeQuota.Read(quotaContext, target.credential, target.connectorID, forceRefresh)
				target.result = accountBridgeQuotaReadResult{report: report, ok: readErr == nil}
			}
		}()
	}
sendTargets:
	for _, target := range targets {
		select {
		case targetChannel <- target:
		case <-quotaContext.Done():
			break sendTargets
		}
	}
	close(targetChannel)
	waitGroup.Wait()

	for index, binding := range bindings {
		if !binding.valid {
			continue
		}
		result := targets[binding.targetKey].result
		if !result.ok {
			continue
		}
		providerSnapshot, ok := result.report.ForModel(binding.modelID)
		if !ok || (providerSnapshot.State == "unknown" && providerSnapshot.RemainingPercent == nil && len(providerSnapshot.Windows) == 0) {
			continue
		}
		usage[index] = mergeFacadeUsageWithProvider(usage[index], providerSnapshot)
	}
	return usage
}

func mergeFacadeUsageWithProvider(runtime facadeUsage, provider accountbridgequota.Snapshot) facadeUsage {
	merged := runtime
	merged.State = provider.State
	merged.RemainingPercent = cloneFloat64Pointer(provider.RemainingPercent)
	merged.LimitingWindowLabel = cloneStringPointer(provider.LimitingWindowLabel)
	merged.ResetsAt = facadeQuotaTimePointer(provider.ResetsAt)
	merged.Windows = make([]facadeUsageWindow, 0, len(provider.Windows)+len(runtime.Windows))
	for _, providerWindow := range provider.Windows {
		merged.Windows = append(merged.Windows, facadeUsageWindow{
			Label:            providerWindow.Label,
			Limit:            exactUint64Pointer(providerWindow.Limit),
			Used:             exactUint64Pointer(providerWindow.Used),
			RemainingPercent: cloneFloat64Pointer(providerWindow.RemainingPercent),
			ResetsAt:         facadeQuotaTimePointer(providerWindow.ResetsAt),
		})
	}
	merged.Windows = append(merged.Windows, runtime.Windows...)
	if !provider.ObservedAt.IsZero() {
		merged.ObservedAt = provider.ObservedAt.UTC().Format(time.RFC3339Nano)
	}

	// Runtime evidence is newer and model-specific. It can make an otherwise
	// available provider report exhausted or temporarily unavailable. A direct
	// provider exhaustion remains authoritative over a generic runtime cooldown.
	switch runtime.State {
	case "exhausted":
		merged.State = "exhausted"
		// Model-specific runtime quota evidence must not render a stale
		// positive account-wide percentage. Provider windows remain available
		// in the detailed view, while the top-level cell correctly renders 0%.
		merged.RemainingPercent = nil
		merged.ResetsAt = cloneStringPointer(runtime.ResetsAt)
		merged.LimitingWindowLabel = cloneStringPointer(runtime.LimitingWindowLabel)
	case "cooldown":
		if provider.State != "exhausted" {
			merged.State = "cooldown"
			// A reliable percentage remains the model-selector value during a
			// temporary cooldown. With no percentage, surface the cooldown reset
			// at the top level; its window is retained in either case.
			if provider.RemainingPercent == nil {
				merged.ResetsAt = cloneStringPointer(runtime.ResetsAt)
				merged.LimitingWindowLabel = cloneStringPointer(runtime.LimitingWindowLabel)
			}
		}
	case "available":
		// An active runtime only proves that a prior request was usable; it does
		// not establish applicability of a newly observed/unbound provider quota
		// window. Preserve provider unknown rather than upgrading uncertainty to
		// a positive availability assertion.
	case "unknown":
		// A provider's direct quota evidence is more useful than an unknown
		// runtime state, so the provider projection is retained.
	}
	return merged
}

func exactUint64Pointer(value *float64) *uint64 {
	if value == nil || math.IsNaN(*value) || math.IsInf(*value, 0) || *value < 0 || *value > math.MaxUint64 || math.Trunc(*value) != *value {
		return nil
	}
	converted := uint64(*value)
	return &converted
}

func facadeQuotaTimePointer(value *time.Time) *string {
	if value == nil || value.IsZero() {
		return nil
	}
	formatted := value.UTC().Format(time.RFC3339Nano)
	return &formatted
}

func cloneFloat64Pointer(value *float64) *float64 {
	if value == nil {
		return nil
	}
	copyValue := *value
	return &copyValue
}

func cloneStringPointer(value *string) *string {
	if value == nil {
		return nil
	}
	copyValue := *value
	return &copyValue
}

func (s *Server) accountBridgeSnapshot() ([]facadeAccount, []facadeModel, []facadeUsage) {
	if s == nil || s.accountBridgeRuntime == nil {
		return []facadeAccount{}, []facadeModel{}, []facadeUsage{}
	}
	routes := s.accountBridgeRuntime.Routes()
	manager := s.accountBridgeRuntime.AuthManager()
	if routes == nil || manager == nil {
		return []facadeAccount{}, []facadeModel{}, []facadeUsage{}
	}

	accounts := make([]facadeAccount, 0)
	models := make([]facadeModel, 0)
	usage := make([]facadeUsage, 0)
	observedAt := time.Now().UTC()
	for _, credential := range accountbridge.SortedAuths(manager) {
		if coreauth.IsPluginVirtualAuth(credential) {
			continue
		}
		connectorID, ok := accountbridge.ConnectorForProvider(credential.Provider)
		if !ok || !s.accountBridgeRuntime.AllowsConnector(connectorID) {
			continue
		}
		accountID, err := routes.AccountID(connectorID, credential.ID)
		if err != nil {
			continue
		}
		accountLabel := "Account " + accountID[:8]
		accounts = append(accounts, facadeAccount{
			AccountID:     accountID,
			ConnectorID:   connectorID,
			DisplayLabel:  accountLabel,
			Status:        facadeAccountStatus(credential),
			ConnectedAt:   facadeConnectedAt(credential),
			LastUsedAt:    nil,
			CooldownUntil: facadeCooldownUntil(credential),
		})
		// Blocked connectors may retain existing encrypted credentials, but they
		// must not enter the selectable model catalog or produce route IDs.
		if !s.accountBridgeRuntime.ConnectorEnabled(connectorID) {
			continue
		}

		for _, model := range registry.GetGlobalRegistry().GetModelsForClient(credential.ID) {
			if model == nil || strings.TrimSpace(model.ID) == "" {
				continue
			}
			routeID, err := routes.Register(accountbridge.Binding{
				ConnectorID: connectorID,
				AuthID:      credential.ID,
				ModelID:     model.ID,
			})
			if err != nil {
				continue
			}
			displayName := strings.TrimSpace(model.DisplayName)
			if displayName == "" {
				displayName = strings.TrimSpace(model.Name)
			}
			var displayNamePtr *string
			if displayName != "" {
				displayNameCopy := displayName
				displayNamePtr = &displayNameCopy
			}
			capabilities := facadeModelCapabilities(model)
			models = append(models, facadeModel{
				RouteID:                  routeID,
				AccountID:                accountID,
				ConnectorID:              connectorID,
				ModelID:                  model.ID,
				DisplayName:              displayNamePtr,
				ConnectorLabel:           s.accountBridgeRuntime.ConnectorDisplayName(connectorID),
				AccountLabel:             accountLabel,
				ChatRuntimeSupported:     capabilities.ChatRuntimeSupported,
				SupportsTools:            capabilities.SupportsTools,
				SupportsThinking:         capabilities.SupportsThinking,
				SupportsAdaptiveThinking: capabilities.SupportsAdaptiveThinking,
				SupportsEffort:           capabilities.SupportsEffort,
				SupportsMaxEffort:        capabilities.SupportsMaxEffort,
				SupportsVision:           capabilities.SupportsVision,
				SupportsJSONMode:         capabilities.SupportsJSONMode,
				SupportedThinkingModes:   capabilities.SupportedThinkingModes,
				DefaultThinkingMode:      capabilities.DefaultThinkingMode,
				ContextWindow:            capabilities.ContextWindow,
				MaxOutputTokens:          capabilities.MaxOutputTokens,
			})
			usageProjection := facadeUsageForModel(credential, model.ID, observedAt)
			usage = append(usage, facadeUsage{
				RouteID:             routeID,
				AccountID:           accountID,
				State:               usageProjection.State,
				RemainingPercent:    usageProjection.RemainingPercent,
				LimitingWindowLabel: usageProjection.LimitingWindowLabel,
				ResetsAt:            usageProjection.ResetsAt,
				Windows:             usageProjection.Windows,
				ObservedAt:          observedAt.Format(time.RFC3339Nano),
			})
		}
	}
	return accounts, models, usage
}

func (s *Server) accountBridgeLoginStartFor(connectorID string) gin.HandlerFunc {
	return func(c *gin.Context) { s.rejectAccountBridgeLogin(c, connectorID) }
}

func (s *Server) accountBridgeLoginStart(c *gin.Context) {
	var body struct {
		ConnectorID string `json:"connectorId"`
	}
	if err := c.ShouldBindJSON(&body); err != nil {
		c.JSON(http.StatusBadRequest, gin.H{"error": "invalid_request"})
		return
	}
	s.rejectAccountBridgeLogin(c, body.ConnectorID)
}

func (s *Server) rejectAccountBridgeLogin(c *gin.Context, connectorID string) {
	connectorID = strings.ToLower(strings.TrimSpace(connectorID))
	known := false
	for _, candidate := range accountbridge.ConnectorIDs() {
		if connectorID == candidate {
			known = true
			break
		}
	}
	if !known || s == nil || s.accountBridgeRuntime == nil || !s.accountBridgeRuntime.AllowsConnector(connectorID) {
		c.JSON(http.StatusForbidden, gin.H{"error": "connector_not_eligible"})
		return
	}
	if !s.accountBridgeRuntime.ConnectorEnabled(connectorID) {
		c.JSON(http.StatusForbidden, gin.H{
			"error":       "connector_disabled",
			"connectorId": connectorID,
			"termsStatus": s.accountBridgeRuntime.ConnectorTermsStatus(connectorID),
			"reasonCode":  s.accountBridgeRuntime.ConnectorDisabledReason(connectorID),
		})
		return
	}
	query := c.Request.URL.Query()
	query.Set("is_webui", "true")
	c.Request.URL.RawQuery = query.Encode()
	switch connectorID {
	case accountbridge.ConnectorOpenAI:
		s.mgmt.RequestCodexToken(c)
	case accountbridge.ConnectorAnthropic:
		s.mgmt.RequestAnthropicToken(c)
	case accountbridge.ConnectorXAI:
		s.mgmt.RequestXAIToken(c)
	case accountbridge.ConnectorQwen:
		s.mgmt.RequestQwenToken(c)
	case accountbridge.ConnectorKimi:
		s.mgmt.RequestKimiToken(c)
	case accountbridge.ConnectorZai:
		s.mgmt.RequestZaiToken(c)
	case accountbridge.ConnectorGoogle:
		originalPath := c.Request.URL.Path
		c.Request.URL.Path = "/v0/management/gemini-cli-auth-url"
		handled := s.mgmt.ServePluginAuthURL(c)
		c.Request.URL.Path = originalPath
		if !handled {
			c.JSON(http.StatusServiceUnavailable, gin.H{"error": "fixed_plugin_unavailable"})
		}
	default:
		c.JSON(http.StatusForbidden, gin.H{"error": "connector_not_eligible"})
	}
}

func (s *Server) accountBridgeAccountRemove(c *gin.Context) {
	accountID := strings.TrimSpace(c.Param("accountId"))
	if s == nil || s.accountBridgeRuntime == nil || accountID == "" {
		c.JSON(http.StatusBadRequest, gin.H{"removed": false})
		return
	}
	manager := s.accountBridgeRuntime.AuthManager()
	if manager == nil {
		c.JSON(http.StatusServiceUnavailable, gin.H{"removed": false})
		return
	}
	for _, credential := range accountbridge.SortedAuths(manager) {
		if credential == nil || coreauth.IsPluginVirtualAuth(credential) {
			continue
		}
		connectorID, ok := accountbridge.ConnectorForProvider(credential.Provider)
		if !ok {
			continue
		}
		candidate, err := s.accountBridgeRuntime.Routes().AccountID(connectorID, credential.ID)
		if err != nil || candidate != accountID {
			continue
		}
		if err = sdkauth.GetTokenStore().Delete(context.Background(), credential.ID); err != nil {
			c.JSON(http.StatusInternalServerError, gin.H{"removed": false})
			return
		}
		s.accountBridgeRuntime.Routes().RemoveAccount(connectorID, credential.ID)
		manager.Remove(context.Background(), credential.ID)
		c.JSON(http.StatusOK, gin.H{"removed": true})
		return
	}
	c.JSON(http.StatusOK, gin.H{"removed": false})
}

func (s *Server) accountBridgeLoginPoll(c *gin.Context) {
	state := strings.TrimSpace(c.Query("state"))
	if state == "" || managementHandlers.ValidateOAuthState(state) != nil {
		c.JSON(http.StatusBadRequest, gin.H{"error": "invalid_session"})
		return
	}
	// Fixed plugin OAuth flows are poll-driven. Advance one provider poll before
	// projecting the private session state; built-in browser/device flows are a
	// no-op here and complete from their background callback waiter.
	s.mgmt.AdvancePluginOAuthSession(c, state)
	provider, status, _, _, completed, ok := managementHandlers.GetOAuthSessionDetails(state)
	response := gin.H{"state": "expired", "accountId": nil, "errorCode": nil}
	if ok {
		switch {
		case completed:
			if accountID, associated := s.accountBridgeCompletedLoginAccountID(state, provider); associated {
				response["state"] = "succeeded"
				response["accountId"] = accountID
			} else {
				response["state"] = "failed"
				response["errorCode"] = "login_account_association_unavailable"
			}
		case status != "":
			response["state"] = "failed"
			// V2: bounded enum transparency — the writer's classified code
			// when present, the historical residual otherwise. The enum is
			// pinned by the cross-language fixture; this branch must never
			// invent a value.
			if code := managementHandlers.GetOAuthSessionErrorCode(state); code != "" {
				response["errorCode"] = code
			} else {
				response["errorCode"] = "login_failed"
			}
		default:
			response["state"] = "pending"
		}
	}
	c.JSON(http.StatusOK, response)
}

// accountBridgeCompletedLoginAccountID converts the exact private credential
// identity captured by OAuth completion into the stable opaque accountId. It
// deliberately refuses missing, multiple, stale, virtual, or cross-provider
// results: guessing from an account-list before/after diff can misassociate a
// concurrent login and cannot represent same-subject reauthorization.
func (s *Server) accountBridgeCompletedLoginAccountID(state, provider string) (string, bool) {
	if s == nil || s.accountBridgeRuntime == nil {
		return "", false
	}
	authIDs, completed, ok := managementHandlers.GetOAuthSessionResultAuthIDs(state)
	if !ok || !completed || len(authIDs) != 1 {
		return "", false
	}
	connectorID, ok := accountbridge.ConnectorForProvider(provider)
	if !ok || !s.accountBridgeRuntime.AllowsConnector(connectorID) {
		return "", false
	}
	manager := s.accountBridgeRuntime.AuthManager()
	if manager == nil {
		return "", false
	}
	credential, ok := manager.GetByID(authIDs[0])
	if !ok || credential == nil || coreauth.IsPluginVirtualAuth(credential) {
		return "", false
	}
	actualConnectorID, ok := accountbridge.ConnectorForProvider(credential.Provider)
	if !ok || actualConnectorID != connectorID {
		return "", false
	}
	accountID, err := s.accountBridgeRuntime.Routes().AccountID(connectorID, credential.ID)
	if err != nil {
		return "", false
	}
	return accountID, true
}

func (s *Server) accountBridgeLoginCancel(c *gin.Context) {
	state := strings.TrimSpace(c.Query("state"))
	if state == "" || managementHandlers.ValidateOAuthState(state) != nil {
		c.JSON(http.StatusBadRequest, gin.H{"error": "invalid_session"})
		return
	}
	c.JSON(http.StatusOK, gin.H{"cancelled": managementHandlers.CancelOAuthSession(state)})
}

func (s *Server) accountBridgeInferenceMiddleware() gin.HandlerFunc {
	return func(c *gin.Context) {
		if s == nil || s.accountBridgeRuntime == nil || !s.accountBridgeRuntime.Decision().BootAllowed() {
			c.AbortWithStatusJSON(http.StatusForbidden, gin.H{"error": "eligibility_denied"})
			return
		}

		routeID := strings.TrimSpace(c.GetHeader(accountRouteHeader))
		c.Request.Header.Del(accountRouteHeader)
		if routeID == "" {
			c.AbortWithStatusJSON(http.StatusBadRequest, gin.H{"error": "missing_account_route"})
			return
		}
		binding, err := s.accountBridgeRuntime.Routes().Resolve(routeID)
		if err != nil {
			c.AbortWithStatusJSON(http.StatusNotFound, gin.H{"error": "unknown_account_route"})
			return
		}
		if !s.accountBridgeRuntime.AllowsConnector(binding.ConnectorID) {
			c.AbortWithStatusJSON(http.StatusForbidden, gin.H{"error": "connector_not_eligible"})
			return
		}
		manager := s.accountBridgeRuntime.AuthManager()
		if manager == nil {
			c.AbortWithStatusJSON(http.StatusServiceUnavailable, gin.H{"error": "account_runtime_unavailable"})
			return
		}
		credential, ok := manager.GetByID(binding.AuthID)
		if !ok || credential == nil || coreauth.IsPluginVirtualAuth(credential) {
			c.AbortWithStatusJSON(http.StatusNotFound, gin.H{"error": "stale_account_route"})
			return
		}
		actualConnector, ok := accountbridge.ConnectorForProvider(credential.Provider)
		if !ok || actualConnector != binding.ConnectorID {
			c.AbortWithStatusJSON(http.StatusConflict, gin.H{"error": "stale_account_route"})
			return
		}
		modelStillRegistered := false
		for _, model := range registry.GetGlobalRegistry().GetModelsForClient(binding.AuthID) {
			if model != nil && model.ID == binding.ModelID {
				modelStillRegistered = true
				break
			}
		}
		if !modelStillRegistered {
			c.AbortWithStatusJSON(http.StatusNotFound, gin.H{"error": "stale_account_route"})
			return
		}

		raw, err := io.ReadAll(io.LimitReader(c.Request.Body, maxAccountBridgeBodySize+1))
		if err != nil || len(raw) > maxAccountBridgeBodySize {
			c.AbortWithStatusJSON(http.StatusBadRequest, gin.H{"error": "invalid_request"})
			return
		}
		c.Request.Body = io.NopCloser(bytes.NewReader(raw))
		model := gjson.GetBytes(raw, "model")
		if !gjson.ValidBytes(raw) || !model.Exists() || model.Type != gjson.String || strings.TrimSpace(model.String()) == "" {
			c.AbortWithStatusJSON(http.StatusBadRequest, gin.H{"error": "missing_model"})
			return
		}
		if model.String() != binding.ModelID {
			c.AbortWithStatusJSON(http.StatusConflict, gin.H{"error": "route_model_mismatch"})
			return
		}
		if !s.accountBridgeRuntime.ConnectorEnabled(binding.ConnectorID) {
			c.AbortWithStatusJSON(http.StatusForbidden, gin.H{"error": "connector_disabled"})
			return
		}

		requestContext := handlers.WithPinnedAuthID(c.Request.Context(), binding.AuthID)
		c.Request = c.Request.WithContext(requestContext)
		c.Next()
	}
}

func facadeAccountStatus(credential *coreauth.Auth) string {
	if credential == nil {
		return "reauthorization-required"
	}
	if credential.Disabled || credential.Status == coreauth.StatusDisabled {
		return "disabled"
	}
	if credential.Quota.Exceeded {
		return "quota-exhausted"
	}
	if credential.Unavailable || (!credential.NextRetryAfter.IsZero() && credential.NextRetryAfter.After(time.Now())) {
		return "cooldown"
	}
	if credential.Status == coreauth.StatusActive || credential.Status == "" {
		return "ready"
	}
	return "reauthorization-required"
}

func facadeConnectedAt(credential *coreauth.Auth) string {
	if credential == nil {
		return time.Time{}.UTC().Format(time.RFC3339)
	}
	connectedAt := credential.CreatedAt
	if connectedAt.IsZero() {
		connectedAt = credential.UpdatedAt
	}
	return connectedAt.UTC().Format(time.RFC3339Nano)
}

func facadeCooldownUntil(credential *coreauth.Auth) *string {
	if credential == nil {
		return nil
	}
	until := credential.NextRetryAfter
	if credential.Quota.NextRecoverAt.After(until) {
		until = credential.Quota.NextRecoverAt
	}
	if until.IsZero() || !until.After(time.Now()) {
		return nil
	}
	formatted := until.UTC().Format(time.RFC3339Nano)
	return &formatted
}

type facadeModelCapabilityProjection struct {
	ChatRuntimeSupported     *bool
	SupportsTools            *bool
	SupportsThinking         *bool
	SupportsAdaptiveThinking *bool
	SupportsEffort           *bool
	SupportsMaxEffort        *bool
	SupportsVision           *bool
	SupportsJSONMode         *bool
	SupportedThinkingModes   []string
	DefaultThinkingMode      *string
	ContextWindow            *uint64
	MaxOutputTokens          *uint64
}

func facadeModelCapabilities(model *registry.ModelInfo) facadeModelCapabilityProjection {
	if model == nil {
		return facadeModelCapabilityProjection{SupportedThinkingModes: []string{}}
	}
	// Capability fields are evidence projections, not provider/model-name
	// guesses. An absent upstream field remains null so callers cannot mistake
	// an unknown ability for a conformance result.
	var chat *bool
	if model.Type == registry.OpenAIImageModelType {
		chat = boolPointer(false)
	} else if len(model.SupportedGenerationMethods) > 0 {
		chat = boolPointer(
			containsFold(model.SupportedGenerationMethods, "generateContent") ||
				containsFold(model.SupportedGenerationMethods, "chat.completions") ||
				containsFold(model.SupportedGenerationMethods, "messages"),
		)
	} else if len(model.SupportedInputModalities) > 0 && len(model.SupportedOutputModalities) > 0 {
		chat = boolPointer(containsFold(model.SupportedInputModalities, "text") && containsFold(model.SupportedOutputModalities, "text"))
	}
	var tools *bool
	var jsonMode *bool
	if len(model.SupportedParameters) > 0 {
		tools = boolPointer(
			containsFold(model.SupportedParameters, "tools") ||
				containsFold(model.SupportedParameters, "tool_choice") ||
				containsFold(model.SupportedParameters, "functions") ||
				containsFold(model.SupportedParameters, "function_call"),
		)
		jsonMode = boolPointer(containsFold(model.SupportedParameters, "response_format") || containsFold(model.SupportedParameters, "json_mode"))
	}
	var vision *bool
	if len(model.SupportedInputModalities) > 0 {
		vision = boolPointer(containsFold(model.SupportedInputModalities, "image"))
	}
	var thinking *bool
	var adaptive *bool
	var effort *bool
	var maxEffort *bool
	modes := []string{"auto"}
	var defaultMode *string
	if model.Thinking != nil {
		thinking = boolPointer(true)
		adaptive = boolPointer(model.Thinking.DynamicAllowed)
		effort = boolPointer(len(model.Thinking.Levels) > 0)
		hasMaxEffort := false
		hasOff := model.Thinking.ZeroAllowed
		for _, level := range model.Thinking.Levels {
			switch strings.ToLower(strings.TrimSpace(level)) {
			case "max", "xhigh":
				hasMaxEffort = true
			case "none", "off", "disabled":
				hasOff = true
			}
		}
		maxEffort = boolPointer(hasMaxEffort)
		if hasOff {
			modes = append(modes, "off")
		}
		modes = append(modes, "standard")
		if hasMaxEffort {
			modes = append(modes, "deep")
		}
		defaultModeValue := "auto"
		defaultMode = &defaultModeValue
	}
	contextWindow := positiveUint64(model.ContextLength)
	if contextWindow == nil {
		contextWindow = positiveUint64(model.InputTokenLimit)
	}
	maxOutputTokens := positiveUint64(model.OutputTokenLimit)
	if maxOutputTokens == nil {
		maxOutputTokens = positiveUint64(model.MaxCompletionTokens)
	}
	return facadeModelCapabilityProjection{
		ChatRuntimeSupported:     chat,
		SupportsTools:            tools,
		SupportsThinking:         thinking,
		SupportsAdaptiveThinking: adaptive,
		SupportsEffort:           effort,
		SupportsMaxEffort:        maxEffort,
		SupportsVision:           vision,
		SupportsJSONMode:         jsonMode,
		SupportedThinkingModes:   modes,
		DefaultThinkingMode:      defaultMode,
		ContextWindow:            contextWindow,
		MaxOutputTokens:          maxOutputTokens,
	}
}

func boolPointer(value bool) *bool {
	return &value
}

func containsFold(values []string, expected string) bool {
	for _, value := range values {
		if strings.EqualFold(strings.TrimSpace(value), expected) {
			return true
		}
	}
	return false
}

func positiveUint64(value int) *uint64 {
	if value <= 0 {
		return nil
	}
	converted := uint64(value)
	return &converted
}

type facadeUsageProjection struct {
	State               string
	RemainingPercent    *float64
	LimitingWindowLabel *string
	ResetsAt            *string
	Windows             []facadeUsageWindow
}

// facadeUsageForModel projects only state already held by the authenticated
// runtime. ModelState wins over aggregated Auth state so one exhausted model
// cannot make a different model on the same account look exhausted. If no
// model-specific evidence exists, account-level evidence is the conservative
// fallback.
//
// QuotaState contains exceeded/recovery/backoff evidence, not a subscription
// limit or used amount. Therefore this projector intentionally leaves every
// percentage nil. A percentage may only be populated later by an audited
// provider adapter that supplies a direct percentage or one same-window
// limit+used pair.
func facadeUsageForModel(credential *coreauth.Auth, modelID string, now time.Time) facadeUsageProjection {
	unknown := facadeUsageProjection{State: "unknown", Windows: []facadeUsageWindow{}}
	if credential == nil || credential.Disabled || credential.Status == coreauth.StatusDisabled {
		return unknown
	}
	if credential.Status != "" && credential.Status != coreauth.StatusActive && credential.Status != coreauth.StatusError {
		return unknown
	}
	if state, ok := credential.ModelStates[modelID]; ok {
		if state == nil {
			return unknown
		}
		return facadeUsageFromRuntimeState(
			state.Status,
			state.Unavailable,
			state.NextRetryAfter,
			state.Quota,
			"model-runtime",
			now,
		)
	}
	return facadeUsageFromRuntimeState(
		credential.Status,
		credential.Unavailable,
		credential.NextRetryAfter,
		credential.Quota,
		"account-runtime",
		now,
	)
}

func facadeUsageFromRuntimeState(
	status coreauth.Status,
	unavailable bool,
	nextRetryAfter time.Time,
	quota coreauth.QuotaState,
	windowPrefix string,
	now time.Time,
) facadeUsageProjection {
	windows := []facadeUsageWindow{}
	if status == coreauth.StatusDisabled || status == coreauth.StatusPending || status == coreauth.StatusRefreshing || status == coreauth.StatusUnknown {
		return facadeUsageProjection{State: "unknown", Windows: windows}
	}

	resetAt := latestFutureTime(now, nextRetryAfter, quota.NextRecoverAt)
	resetAtString := facadeTimePointer(resetAt)
	quotaStillActive := quota.Exceeded && (quota.NextRecoverAt.IsZero() || quota.NextRecoverAt.After(now))
	if quotaStillActive {
		label := windowPrefix + "-quota"
		windows = append(windows, facadeUsageWindow{
			Label:            label,
			Limit:            nil,
			Used:             nil,
			RemainingPercent: nil,
			ResetsAt:         resetAtString,
		})
		return facadeUsageProjection{
			State:               "exhausted",
			RemainingPercent:    nil,
			LimitingWindowLabel: &label,
			ResetsAt:            resetAtString,
			Windows:             windows,
		}
	}

	if unavailable || !resetAt.IsZero() {
		label := windowPrefix + "-cooldown"
		windows = append(windows, facadeUsageWindow{
			Label:            label,
			Limit:            nil,
			Used:             nil,
			RemainingPercent: nil,
			ResetsAt:         resetAtString,
		})
		return facadeUsageProjection{
			State:               "cooldown",
			RemainingPercent:    nil,
			LimitingWindowLabel: &label,
			ResetsAt:            resetAtString,
			Windows:             windows,
		}
	}

	if status == "" || status == coreauth.StatusActive {
		return facadeUsageProjection{State: "available", Windows: windows}
	}
	return facadeUsageProjection{State: "unknown", Windows: windows}
}

func latestFutureTime(now time.Time, values ...time.Time) time.Time {
	latest := time.Time{}
	for _, value := range values {
		if value.After(now) && value.After(latest) {
			latest = value
		}
	}
	return latest
}

func facadeTimePointer(value time.Time) *string {
	if value.IsZero() {
		return nil
	}
	formatted := value.UTC().Format(time.RFC3339Nano)
	return &formatted
}
