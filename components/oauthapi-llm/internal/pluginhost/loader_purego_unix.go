//go:build linux || freebsd

package pluginhost

import (
	"context"
	"fmt"
	"sync"
	"sync/atomic"
	"unsafe"

	"github.com/ebitengine/purego"
)

// The structures below intentionally spell out the cliproxy C ABI. All
// supported purego targets are 64-bit, so the Go compiler's four bytes of
// padding after abiVersion match the C compiler's pointer alignment.
type puregoBuffer struct {
	ptr unsafe.Pointer
	len uintptr
}

type puregoHostAPI struct {
	abiVersion uint32
	hostCtx    unsafe.Pointer
	call       uintptr
	freeBuffer uintptr
}

type puregoPluginAPI struct {
	abiVersion uint32
	call       uintptr
	freeBuffer uintptr
	shutdown   uintptr
}

var (
	puregoHostCallbackID      atomic.Uintptr
	puregoHostCallbackEntries sync.Map
	puregoHostCallCallback    = purego.NewCallback(puregoHostCall)
	puregoHostFreeCallback    = purego.NewCallback(puregoHostFree)
	puregoMalloc              func(uintptr) unsafe.Pointer
	puregoFree                func(unsafe.Pointer)
)

func init() {
	purego.RegisterLibFunc(&puregoMalloc, purego.RTLD_DEFAULT, "malloc")
	purego.RegisterLibFunc(&puregoFree, purego.RTLD_DEFAULT, "free")
}

type dynamicLibraryLoader struct{}

type dynamicLibraryClient struct {
	handle  uintptr
	hostAPI unsafe.Pointer
	hostCtx unsafe.Pointer
	api     unsafe.Pointer

	call       func(unsafe.Pointer, unsafe.Pointer, uintptr, unsafe.Pointer) int32
	freeBuffer func(unsafe.Pointer, uintptr)
	shutdown   func()
}

func defaultPluginLoader() pluginLoader {
	return dynamicLibraryLoader{}
}

func (dynamicLibraryLoader) Open(file pluginFile, host *Host) (pluginClient, error) {
	handle, errOpen := purego.Dlopen(file.Path, purego.RTLD_NOW|purego.RTLD_LOCAL)
	if errOpen != nil {
		return nil, fmt.Errorf("dlopen %s: %w", file.Path, errOpen)
	}
	initSymbol, errSymbol := purego.Dlsym(handle, "cliproxy_plugin_init")
	if errSymbol != nil {
		_ = purego.Dlclose(handle)
		return nil, fmt.Errorf("missing cliproxy_plugin_init: %w", errSymbol)
	}

	hostAPI := puregoMalloc(unsafe.Sizeof(puregoHostAPI{}))
	if hostAPI == nil {
		_ = purego.Dlclose(handle)
		return nil, fmt.Errorf("allocate host api")
	}
	hostCtx := puregoMalloc(unsafe.Sizeof(uintptr(0)))
	if hostCtx == nil {
		puregoFree(hostAPI)
		_ = purego.Dlclose(handle)
		return nil, fmt.Errorf("allocate host context")
	}

	id := puregoHostCallbackID.Add(1)
	*(*uintptr)(hostCtx) = id
	puregoHostCallbackEntries.Store(id, dynamicHostCallbackEntry{host: host, pluginID: file.ID})
	*(*puregoHostAPI)(hostAPI) = puregoHostAPI{
		abiVersion: pluginHostABIVersion,
		hostCtx:    hostCtx,
		call:       puregoHostCallCallback,
		freeBuffer: puregoHostFreeCallback,
	}

	client := &dynamicLibraryClient{
		handle:  handle,
		hostAPI: hostAPI,
		hostCtx: hostCtx,
	}
	client.api = puregoMalloc(unsafe.Sizeof(puregoPluginAPI{}))
	if client.api == nil {
		client.Shutdown()
		return nil, fmt.Errorf("allocate plugin api")
	}
	var initialize func(unsafe.Pointer, unsafe.Pointer) int32
	purego.RegisterFunc(&initialize, initSymbol)
	rc := initialize(hostAPI, client.api)
	if rc != 0 {
		client.Shutdown()
		return nil, fmt.Errorf("cliproxy_plugin_init returned %d", rc)
	}
	api := (*puregoPluginAPI)(client.api)
	if api.abiVersion != pluginHostABIVersion {
		client.Shutdown()
		return nil, fmt.Errorf("plugin ABI version %d is not supported", api.abiVersion)
	}
	if api.call == 0 || api.freeBuffer == 0 {
		client.Shutdown()
		return nil, fmt.Errorf("plugin function table is incomplete")
	}
	purego.RegisterFunc(&client.call, api.call)
	purego.RegisterFunc(&client.freeBuffer, api.freeBuffer)
	if api.shutdown != 0 {
		purego.RegisterFunc(&client.shutdown, api.shutdown)
	}
	return client, nil
}

func (c *dynamicLibraryClient) Call(ctx context.Context, method string, request []byte) ([]byte, error) {
	if c == nil || c.call == nil {
		return nil, fmt.Errorf("plugin client is closed")
	}
	if ctx != nil {
		select {
		case <-ctx.Done():
			return nil, ctx.Err()
		default:
		}
	}

	methodPtr := puregoCopyCString(method)
	if methodPtr == nil {
		return nil, fmt.Errorf("allocate plugin method")
	}
	defer puregoFree(methodPtr)
	var requestPtr unsafe.Pointer
	if len(request) > 0 {
		requestPtr = puregoMalloc(uintptr(len(request)))
		if requestPtr == nil {
			return nil, fmt.Errorf("allocate plugin request")
		}
		defer puregoFree(requestPtr)
		copy(unsafe.Slice((*byte)(requestPtr), len(request)), request)
	}

	responsePtr := puregoMalloc(unsafe.Sizeof(puregoBuffer{}))
	if responsePtr == nil {
		return nil, fmt.Errorf("allocate plugin response descriptor")
	}
	defer puregoFree(responsePtr)
	response := (*puregoBuffer)(responsePtr)
	rc := c.call(methodPtr, requestPtr, uintptr(len(request)), responsePtr)
	var out []byte
	if response.ptr != nil && response.len > 0 {
		if response.len > uintptr(maxIntValue()) {
			c.freeBuffer(response.ptr, response.len)
			return nil, fmt.Errorf("plugin response is too large: %d", response.len)
		}
		out = append([]byte(nil), unsafe.Slice((*byte)(response.ptr), int(response.len))...)
	}
	if response.ptr != nil {
		c.freeBuffer(response.ptr, response.len)
	}
	if rc != 0 {
		if isPluginErrorEnvelope(out) {
			return out, nil
		}
		return nil, fmt.Errorf("plugin call %s returned %d: %s", method, rc, string(out))
	}
	return out, nil
}

func (c *dynamicLibraryClient) Shutdown() {
	if c == nil {
		return
	}
	if c.shutdown != nil {
		c.shutdown()
		c.shutdown = nil
	}
	if c.api != nil {
		puregoFree(c.api)
		c.api = nil
	}
	if c.hostCtx != nil {
		id := *(*uintptr)(c.hostCtx)
		puregoHostCallbackEntries.Delete(id)
		puregoFree(c.hostCtx)
		c.hostCtx = nil
	}
	if c.hostAPI != nil {
		puregoFree(c.hostAPI)
		c.hostAPI = nil
	}
	if c.handle != 0 {
		_ = purego.Dlclose(c.handle)
		c.handle = 0
	}
	c.call = nil
	c.freeBuffer = nil
}

func puregoHostCall(hostCtx unsafe.Pointer, methodPtr unsafe.Pointer, requestPtr unsafe.Pointer, requestLen uintptr, responsePtr unsafe.Pointer) int32 {
	if responsePtr != nil {
		response := (*puregoBuffer)(responsePtr)
		response.ptr = nil
		response.len = 0
	}
	if hostCtx == nil || methodPtr == nil || requestLen > uintptr(maxIntValue()) {
		return 1
	}
	id := *(*uintptr)(hostCtx)
	rawHost, okHost := puregoHostCallbackEntries.Load(id)
	if !okHost {
		return 1
	}
	entry, okHost := rawHost.(dynamicHostCallbackEntry)
	if !okHost || entry.host == nil {
		return 1
	}
	method, okMethod := puregoReadCString(methodPtr, 4096)
	if !okMethod {
		return 1
	}
	var request []byte
	if requestPtr != nil && requestLen > 0 {
		request = append([]byte(nil), unsafe.Slice((*byte)(requestPtr), int(requestLen))...)
	}
	ctx := withHostCallbackPluginID(context.Background(), entry.pluginID)
	resp, errCall := entry.host.callFromPlugin(ctx, method, request)
	if errCall != nil {
		resp = marshalRPCError("host_call_failed", errCall.Error())
	}
	if len(resp) == 0 || responsePtr == nil {
		return 0
	}
	ptr := puregoMalloc(uintptr(len(resp)))
	if ptr == nil {
		return 1
	}
	copy(unsafe.Slice((*byte)(ptr), len(resp)), resp)
	response := (*puregoBuffer)(responsePtr)
	response.ptr = ptr
	response.len = uintptr(len(resp))
	return 0
}

func puregoHostFree(ptr unsafe.Pointer, _ uintptr) {
	if ptr != nil {
		puregoFree(ptr)
	}
}

func puregoCopyCString(value string) unsafe.Pointer {
	ptr := puregoMalloc(uintptr(len(value) + 1))
	if ptr == nil {
		return nil
	}
	bytes := unsafe.Slice((*byte)(ptr), len(value)+1)
	copy(bytes, value)
	bytes[len(value)] = 0
	return ptr
}

func puregoReadCString(ptr unsafe.Pointer, limit int) (string, bool) {
	if ptr == nil || limit <= 0 {
		return "", false
	}
	for index := 0; index < limit; index++ {
		if *(*byte)(unsafe.Add(ptr, index)) == 0 {
			return string(unsafe.Slice((*byte)(ptr), index)), true
		}
	}
	return "", false
}

func maxIntValue() int {
	return int(^uint(0) >> 1)
}
