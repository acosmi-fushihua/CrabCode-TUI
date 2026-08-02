//go:build darwin

package pluginhost

import (
	"bufio"
	"bytes"
	"context"
	"encoding/binary"
	"os"
	"os/exec"
	"strings"
	"testing"
	"time"
)

func TestDarwinPluginHelperProtocolRoundTrip(t *testing.T) {
	want := darwinFrame{typeID: darwinFrameHostCall, method: "host.log", payload: []byte(`{"level":"debug"}`)}
	var encoded bytes.Buffer
	if errWrite := writeDarwinFrame(&encoded, want); errWrite != nil {
		t.Fatal(errWrite)
	}
	got, errRead := readDarwinFrame(&encoded)
	if errRead != nil {
		t.Fatal(errRead)
	}
	if got.typeID != want.typeID || got.method != want.method || !bytes.Equal(got.payload, want.payload) {
		t.Fatalf("round trip = %#v, want %#v", got, want)
	}
}

func TestDarwinPluginHelperProtocolRejectsOversizedFrame(t *testing.T) {
	var encoded bytes.Buffer
	header := make([]byte, 16)
	copy(header[0:4], darwinFrameMagic[:])
	header[4] = darwinProtocolVersion
	header[5] = darwinFrameCall
	binary.BigEndian.PutUint32(header[8:12], darwinMaxFieldBytes+1)
	encoded.Write(header)
	if _, errRead := readDarwinFrame(&encoded); errRead == nil {
		t.Fatal("oversized frame was accepted")
	}
}

func TestDarwinPluginHelperStatusPayloadRoundTrip(t *testing.T) {
	want := []byte(`{"ok":true}`)
	status, body, errParse := parseDarwinStatusPayload(darwinStatusPayload(7, want))
	if errParse != nil {
		t.Fatal(errParse)
	}
	if status != 7 || !bytes.Equal(body, want) {
		t.Fatalf("status/body = %d/%s", status, body)
	}
}

func TestDarwinPluginCallbackFailureUnblocksMainCall(t *testing.T) {
	mainChildInput, mainParentInput, errPipe := os.Pipe()
	if errPipe != nil {
		t.Fatal(errPipe)
	}
	mainParentOutput, mainChildOutput, errPipe := os.Pipe()
	if errPipe != nil {
		t.Fatal(errPipe)
	}
	callbackChildInput, callbackParentInput, errPipe := os.Pipe()
	if errPipe != nil {
		t.Fatal(errPipe)
	}
	callbackParentOutput, callbackChildOutput, errPipe := os.Pipe()
	if errPipe != nil {
		t.Fatal(errPipe)
	}
	for _, file := range []*os.File{
		mainChildInput,
		mainParentInput,
		mainParentOutput,
		mainChildOutput,
		callbackChildInput,
		callbackParentInput,
		callbackParentOutput,
		callbackChildOutput,
	} {
		file := file
		t.Cleanup(func() { _ = file.Close() })
	}

	client := &dynamicLibraryClient{
		cmd:          &exec.Cmd{},
		input:        mainParentInput,
		output:       mainParentOutput,
		reader:       bufio.NewReader(mainParentOutput),
		callbackDone: make(chan struct{}),
		stdout:       &lockedOutput{},
		stderr:       &lockedOutput{},
		host:         New(),
		pluginID:     "callback-failure-fixture",
	}
	go client.serveCallbacks(
		callbackParentInput,
		callbackParentOutput,
		mainParentInput,
		mainParentOutput,
	)

	callDone := make(chan error, 1)
	go func() {
		_, errCall := client.Call(context.Background(), "test.blocked", nil)
		callDone <- errCall
	}()
	frame, errRead := readDarwinFrame(mainChildInput)
	if errRead != nil {
		t.Fatalf("read blocked main call: %v", errRead)
	}
	if frame.typeID != darwinFrameCall || frame.method != "test.blocked" {
		t.Fatalf("main call frame = %#v", frame)
	}

	// EOF on the callback channel must abort the independent main channel.
	if errClose := callbackChildOutput.Close(); errClose != nil {
		t.Fatal(errClose)
	}
	select {
	case errCall := <-callDone:
		if errCall == nil || !strings.Contains(errCall.Error(), "callback=EOF") {
			t.Fatalf("blocked main call error = %v", errCall)
		}
	case <-time.After(time.Second):
		t.Fatal("main plugin call remained blocked after callback channel failure")
	}
	select {
	case <-client.callbackDone:
	case <-time.After(time.Second):
		t.Fatal("callback server did not stop after channel failure")
	}
}
