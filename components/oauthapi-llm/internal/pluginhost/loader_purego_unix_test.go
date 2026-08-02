//go:build linux || freebsd

package pluginhost

import (
	"encoding/json"
	"testing"
	"unsafe"

	"github.com/acosmi/OAuthAPI-LLM/sdk/pluginabi"
	"github.com/ebitengine/purego"
)

func TestPuregoHostCallbackABI(t *testing.T) {
	if unsafe.Sizeof(uintptr(0)) == 8 {
		if unsafe.Sizeof(puregoHostAPI{}) != 32 || unsafe.Offsetof(puregoHostAPI{}.hostCtx) != 8 || unsafe.Sizeof(puregoPluginAPI{}) != 32 {
			t.Fatalf("purego structures do not match the 64-bit cliproxy C ABI")
		}
	}

	host := New()
	id := puregoHostCallbackID.Add(1)
	hostCtx := puregoMalloc(unsafe.Sizeof(uintptr(0)))
	if hostCtx == nil {
		t.Fatal("allocate callback host context")
	}
	defer puregoFree(hostCtx)
	*(*uintptr)(hostCtx) = id
	puregoHostCallbackEntries.Store(id, dynamicHostCallbackEntry{host: host, pluginID: "callback-smoke"})
	defer puregoHostCallbackEntries.Delete(id)

	method := puregoCopyCString(pluginabi.MethodHostLog)
	if method == nil {
		t.Fatal("allocate callback method")
	}
	defer puregoFree(method)
	requestBytes, errMarshal := json.Marshal(rpcHostLogRequest{Level: "debug", Message: "purego callback ABI smoke"})
	if errMarshal != nil {
		t.Fatal(errMarshal)
	}
	request := puregoMalloc(uintptr(len(requestBytes)))
	if request == nil {
		t.Fatal("allocate callback request")
	}
	defer puregoFree(request)
	copy(unsafe.Slice((*byte)(request), len(requestBytes)), requestBytes)
	directResponsePtr := puregoMalloc(unsafe.Sizeof(puregoBuffer{}))
	if directResponsePtr == nil {
		t.Fatal("allocate direct callback response")
	}
	defer puregoFree(directResponsePtr)
	directResponse := (*puregoBuffer)(directResponsePtr)
	if rc := puregoHostCall(hostCtx, method, request, uintptr(len(requestBytes)), directResponsePtr); rc != 0 {
		t.Fatalf("direct host callback returned %d", rc)
	}
	if directResponse.ptr == nil || directResponse.len == 0 {
		t.Fatal("direct host callback returned an empty RPC envelope")
	}
	puregoHostFree(directResponse.ptr, directResponse.len)

	var invoke func(unsafe.Pointer, unsafe.Pointer, unsafe.Pointer, uintptr, unsafe.Pointer) int32
	purego.RegisterFunc(&invoke, puregoHostCallCallback)
	responsePtr := puregoMalloc(unsafe.Sizeof(puregoBuffer{}))
	if responsePtr == nil {
		t.Fatal("allocate callback response")
	}
	defer puregoFree(responsePtr)
	response := (*puregoBuffer)(responsePtr)
	if rc := invoke(hostCtx, method, request, uintptr(len(requestBytes)), responsePtr); rc != 0 {
		t.Fatalf("host callback returned %d", rc)
	}
	if response.ptr == nil || response.len == 0 {
		t.Fatal("host callback returned an empty RPC envelope")
	}
	responseBytes := append([]byte(nil), unsafe.Slice((*byte)(response.ptr), int(response.len))...)
	var envelope pluginabi.Envelope
	if errDecode := json.Unmarshal(responseBytes, &envelope); errDecode != nil || !envelope.OK {
		t.Fatalf("host callback response is invalid: %s (%v)", responseBytes, errDecode)
	}

	var release func(unsafe.Pointer, uintptr)
	purego.RegisterFunc(&release, puregoHostFreeCallback)
	release(response.ptr, response.len)
}
