//go:build darwin

package pluginhost

import (
	"bufio"
	"bytes"
	"context"
	"encoding/binary"
	"fmt"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"sync"
	"time"
)

const (
	darwinProtocolVersion    = byte(1)
	darwinFrameHello         = byte(1)
	darwinFrameCall          = byte(2)
	darwinFrameCallResponse  = byte(3)
	darwinFrameHostCall      = byte(4)
	darwinFrameHostResponse  = byte(5)
	darwinFrameShutdown      = byte(6)
	darwinFrameError         = byte(7)
	darwinMaxFieldBytes      = 64 * 1024 * 1024
	darwinMaxDiagnosticBytes = 1024 * 1024
	darwinHelperName         = "oauthapi-plugin-host"
)

var darwinFrameMagic = [4]byte{'C', 'C', 'P', 'H'}

type darwinFrame struct {
	typeID  byte
	method  string
	payload []byte
}

type lockedOutput struct {
	mu        sync.Mutex
	buf       bytes.Buffer
	truncated bool
}

func (o *lockedOutput) Write(value []byte) (int, error) {
	o.mu.Lock()
	defer o.mu.Unlock()
	originalLen := len(value)
	remaining := darwinMaxDiagnosticBytes - o.buf.Len()
	if remaining <= 0 {
		o.truncated = o.truncated || originalLen > 0
		return originalLen, nil
	}
	if len(value) > remaining {
		value = value[:remaining]
		o.truncated = true
	}
	_, _ = o.buf.Write(value)
	return originalLen, nil
}

func (o *lockedOutput) String() string {
	o.mu.Lock()
	defer o.mu.Unlock()
	value := strings.TrimSpace(o.buf.String())
	if o.truncated {
		value += " [truncated]"
	}
	return value
}

type dynamicLibraryLoader struct{}

type dynamicLibraryClient struct {
	mu             sync.Mutex
	callbackMu     sync.Mutex
	cmd            *exec.Cmd
	input          *os.File
	output         *os.File
	reader         *bufio.Reader
	callbackInput  *os.File
	callbackOutput *os.File
	callbackDone   chan struct{}
	callbackErr    error
	closing        bool
	wait           chan error
	stdout         *lockedOutput
	stderr         *lockedOutput
	host           *Host
	pluginID       string
}

func defaultPluginLoader() pluginLoader {
	return dynamicLibraryLoader{}
}

func (dynamicLibraryLoader) Open(file pluginFile, host *Host) (pluginClient, error) {
	helper, errHelper := darwinPluginHelperPath(file.Path)
	if errHelper != nil {
		return nil, errHelper
	}
	toChildRead, toChildWrite, errPipe := os.Pipe()
	if errPipe != nil {
		return nil, fmt.Errorf("create plugin host input pipe: %w", errPipe)
	}
	fromChildRead, fromChildWrite, errPipe := os.Pipe()
	if errPipe != nil {
		_ = toChildRead.Close()
		_ = toChildWrite.Close()
		return nil, fmt.Errorf("create plugin host output pipe: %w", errPipe)
	}
	callbackFromChildRead, callbackFromChildWrite, errPipe := os.Pipe()
	if errPipe != nil {
		_ = toChildRead.Close()
		_ = toChildWrite.Close()
		_ = fromChildRead.Close()
		_ = fromChildWrite.Close()
		return nil, fmt.Errorf("create plugin host callback output pipe: %w", errPipe)
	}
	callbackToChildRead, callbackToChildWrite, errPipe := os.Pipe()
	if errPipe != nil {
		_ = toChildRead.Close()
		_ = toChildWrite.Close()
		_ = fromChildRead.Close()
		_ = fromChildWrite.Close()
		_ = callbackFromChildRead.Close()
		_ = callbackFromChildWrite.Close()
		return nil, fmt.Errorf("create plugin host callback input pipe: %w", errPipe)
	}
	closePipes := func() {
		_ = toChildRead.Close()
		_ = toChildWrite.Close()
		_ = fromChildRead.Close()
		_ = fromChildWrite.Close()
		_ = callbackFromChildRead.Close()
		_ = callbackFromChildWrite.Close()
		_ = callbackToChildRead.Close()
		_ = callbackToChildWrite.Close()
	}

	stdout := &lockedOutput{}
	stderr := &lockedOutput{}
	cmd := exec.Command(helper, "--plugin", file.Path)
	// fd 3/4 carry serialized plugin calls. fd 5/6 are a dedicated callback
	// channel because the fixed plugin can emit stream callbacks after a call
	// has returned from a background goroutine.
	cmd.ExtraFiles = []*os.File{
		toChildRead,
		fromChildWrite,
		callbackFromChildWrite,
		callbackToChildRead,
	}
	cmd.Stdout = stdout
	cmd.Stderr = stderr
	if errStart := cmd.Start(); errStart != nil {
		closePipes()
		return nil, fmt.Errorf("start macOS plugin helper: %w", errStart)
	}
	_ = toChildRead.Close()
	_ = fromChildWrite.Close()
	_ = callbackFromChildWrite.Close()
	_ = callbackToChildRead.Close()
	client := &dynamicLibraryClient{
		cmd:            cmd,
		input:          toChildWrite,
		output:         fromChildRead,
		reader:         bufio.NewReader(fromChildRead),
		callbackInput:  callbackToChildWrite,
		callbackOutput: callbackFromChildRead,
		callbackDone:   make(chan struct{}),
		wait:           make(chan error, 1),
		stdout:         stdout,
		stderr:         stderr,
		host:           host,
		pluginID:       file.ID,
	}
	go func() {
		client.wait <- cmd.Wait()
	}()
	go client.serveCallbacks(
		callbackToChildWrite,
		callbackFromChildRead,
		toChildWrite,
		fromChildRead,
	)

	frame, errRead := readDarwinFrame(client.reader)
	if errRead != nil {
		client.closeAfterFailure()
		return nil, fmt.Errorf("start macOS plugin helper protocol: %w%s", errRead, client.outputDetail())
	}
	if frame.typeID == darwinFrameError {
		client.closeAfterFailure()
		return nil, fmt.Errorf("load macOS plugin: %s%s", string(frame.payload), client.outputDetail())
	}
	if frame.typeID != darwinFrameHello || frame.method != "" || len(frame.payload) != 4 ||
		binary.BigEndian.Uint32(frame.payload) != pluginHostABIVersion {
		client.closeAfterFailure()
		return nil, fmt.Errorf("macOS plugin helper returned an invalid ABI handshake%s", client.outputDetail())
	}
	return client, nil
}

func darwinPluginHelperPath(pluginPath string) (string, error) {
	candidates := make([]string, 0, 5)
	if configured := strings.TrimSpace(os.Getenv("ACCOUNT_BRIDGE_PLUGIN_HELPER_PATH")); configured != "" {
		candidates = append(candidates, configured)
	}
	if executable, errExecutable := os.Executable(); errExecutable == nil {
		candidates = append(candidates, filepath.Join(filepath.Dir(executable), darwinHelperName))
	}
	pluginDir := filepath.Dir(pluginPath)
	candidates = append(
		candidates,
		filepath.Join(pluginDir, "..", "bin", darwinHelperName),
		filepath.Join(pluginDir, "..", "..", darwinHelperName),
		filepath.Join(pluginDir, darwinHelperName),
	)
	seen := make(map[string]struct{}, len(candidates))
	for _, candidate := range candidates {
		candidate = filepath.Clean(candidate)
		if _, ok := seen[candidate]; ok {
			continue
		}
		seen[candidate] = struct{}{}
		info, errStat := os.Lstat(candidate)
		if errStat != nil {
			continue
		}
		if info.Mode()&os.ModeSymlink != 0 || !info.Mode().IsRegular() || info.Mode().Perm()&0o111 == 0 {
			return "", fmt.Errorf("macOS plugin helper is not a regular executable: %s", candidate)
		}
		return candidate, nil
	}
	return "", fmt.Errorf("macOS plugin helper %s was not found", darwinHelperName)
}

func (c *dynamicLibraryClient) Call(ctx context.Context, method string, request []byte) ([]byte, error) {
	if ctx != nil {
		select {
		case <-ctx.Done():
			return nil, ctx.Err()
		default:
		}
	}
	c.mu.Lock()
	defer c.mu.Unlock()
	if c.cmd == nil || c.input == nil || c.reader == nil {
		return nil, fmt.Errorf("plugin client is closed")
	}
	if errCallback := c.callbackFailure(); errCallback != nil {
		return nil, fmt.Errorf("macOS plugin callback channel failed: %w", errCallback)
	}
	if ctx != nil {
		select {
		case <-ctx.Done():
			return nil, ctx.Err()
		default:
		}
	}
	if errWrite := writeDarwinFrame(c.input, darwinFrame{typeID: darwinFrameCall, method: method, payload: request}); errWrite != nil {
		return nil, fmt.Errorf("write macOS plugin call: %w%s", errWrite, c.outputDetail())
	}
	frame, errRead := readDarwinFrame(c.reader)
	if errRead != nil {
		return nil, fmt.Errorf("read macOS plugin call: %w%s", errRead, c.outputDetail())
	}
	switch frame.typeID {
	case darwinFrameCallResponse:
		status, body, errStatus := parseDarwinStatusPayload(frame.payload)
		if errStatus != nil {
			return nil, errStatus
		}
		if status != 0 {
			if isPluginErrorEnvelope(body) {
				return body, nil
			}
			return nil, fmt.Errorf("plugin call %s returned %d: %s", method, status, string(body))
		}
		return body, nil
	case darwinFrameError:
		return nil, fmt.Errorf("macOS plugin helper failed: %s%s", string(frame.payload), c.outputDetail())
	default:
		return nil, fmt.Errorf("macOS plugin helper returned unexpected call-channel frame type %d", frame.typeID)
	}
}

func (c *dynamicLibraryClient) serveCallbacks(
	input *os.File,
	output *os.File,
	mainInput *os.File,
	mainOutput *os.File,
) {
	defer close(c.callbackDone)
	defer func() {
		_ = input.Close()
		_ = output.Close()
	}()
	fail := func(err error) {
		if !c.recordCallbackFailure(err) {
			return
		}
		// A plugin call may be blocked waiting for this callback response while
		// the parent is blocked waiting for the plugin call response. Closing
		// both main-channel ends wakes the parent immediately and gives the child
		// EOF/EPIPE so the two processes cannot deadlock on a failed callback
		// channel.
		_ = mainInput.Close()
		_ = mainOutput.Close()
	}
	reader := bufio.NewReader(output)
	for {
		frame, errRead := readDarwinFrame(reader)
		if errRead != nil {
			fail(errRead)
			return
		}
		if frame.typeID != darwinFrameHostCall || frame.method == "" {
			fail(fmt.Errorf("unexpected callback-channel frame type %d", frame.typeID))
			return
		}
		callbackCtx := withHostCallbackPluginID(context.Background(), c.pluginID)
		resp, errCall := c.host.callFromPlugin(callbackCtx, frame.method, frame.payload)
		if errCall != nil {
			resp = marshalRPCError("host_call_failed", errCall.Error())
		}
		if errWrite := writeDarwinFrame(input, darwinFrame{
			typeID:  darwinFrameHostResponse,
			payload: darwinStatusPayload(0, resp),
		}); errWrite != nil {
			fail(errWrite)
			return
		}
	}
}

func (c *dynamicLibraryClient) recordCallbackFailure(err error) bool {
	if err == nil {
		return false
	}
	c.callbackMu.Lock()
	defer c.callbackMu.Unlock()
	if c.closing || c.callbackErr != nil {
		return false
	}
	c.callbackErr = err
	return true
}

func (c *dynamicLibraryClient) beginClosing() {
	c.callbackMu.Lock()
	c.closing = true
	c.callbackMu.Unlock()
}

func (c *dynamicLibraryClient) callbackFailure() error {
	c.callbackMu.Lock()
	defer c.callbackMu.Unlock()
	return c.callbackErr
}

func (c *dynamicLibraryClient) Shutdown() {
	if c == nil {
		return
	}
	c.mu.Lock()
	defer c.mu.Unlock()
	if c.cmd == nil {
		return
	}
	c.beginClosing()
	if c.input != nil {
		_ = writeDarwinFrame(c.input, darwinFrame{typeID: darwinFrameShutdown})
		_ = c.input.Close()
		c.input = nil
	}
	select {
	case <-c.wait:
	case <-time.After(5 * time.Second):
		_ = c.cmd.Process.Kill()
		<-c.wait
	}
	c.closeParentPipes()
	c.cmd = nil
}

func (c *dynamicLibraryClient) closeAfterFailure() {
	if c == nil || c.cmd == nil {
		return
	}
	c.beginClosing()
	c.closeParentPipes()
	_ = c.cmd.Process.Kill()
	<-c.wait
	c.cmd = nil
}

func (c *dynamicLibraryClient) closeParentPipes() {
	for _, file := range []*os.File{c.input, c.output, c.callbackInput, c.callbackOutput} {
		if file != nil {
			_ = file.Close()
		}
	}
	c.input = nil
	c.output = nil
	c.reader = nil
	c.callbackInput = nil
	c.callbackOutput = nil
}

func (c *dynamicLibraryClient) outputDetail() string {
	if c == nil {
		return ""
	}
	parts := make([]string, 0, 2)
	if value := c.stderr.String(); value != "" {
		parts = append(parts, "stderr="+value)
	}
	if value := c.stdout.String(); value != "" {
		parts = append(parts, "stdout="+value)
	}
	if err := c.callbackFailure(); err != nil {
		parts = append(parts, "callback="+err.Error())
	}
	if len(parts) == 0 {
		return ""
	}
	return " (" + strings.Join(parts, "; ") + ")"
}

func readDarwinFrame(reader io.Reader) (darwinFrame, error) {
	var header [16]byte
	if _, errRead := io.ReadFull(reader, header[:]); errRead != nil {
		return darwinFrame{}, errRead
	}
	if !bytes.Equal(header[0:4], darwinFrameMagic[:]) || header[4] != darwinProtocolVersion ||
		header[6] != 0 || header[7] != 0 {
		return darwinFrame{}, fmt.Errorf("invalid protocol header")
	}
	methodLen := binary.BigEndian.Uint32(header[8:12])
	payloadLen := binary.BigEndian.Uint32(header[12:16])
	if methodLen > darwinMaxFieldBytes || payloadLen > darwinMaxFieldBytes {
		return darwinFrame{}, fmt.Errorf("protocol frame exceeds size limit")
	}
	methodBytes := make([]byte, int(methodLen))
	if _, errRead := io.ReadFull(reader, methodBytes); errRead != nil {
		return darwinFrame{}, errRead
	}
	if bytes.IndexByte(methodBytes, 0) >= 0 {
		return darwinFrame{}, fmt.Errorf("protocol method contains NUL")
	}
	payload := make([]byte, int(payloadLen))
	if _, errRead := io.ReadFull(reader, payload); errRead != nil {
		return darwinFrame{}, errRead
	}
	return darwinFrame{typeID: header[5], method: string(methodBytes), payload: payload}, nil
}

func writeDarwinFrame(writer io.Writer, frame darwinFrame) error {
	method := []byte(frame.method)
	if len(method) > darwinMaxFieldBytes || len(frame.payload) > darwinMaxFieldBytes {
		return fmt.Errorf("protocol frame exceeds size limit")
	}
	if bytes.IndexByte(method, 0) >= 0 {
		return fmt.Errorf("protocol method contains NUL")
	}
	var header [16]byte
	copy(header[0:4], darwinFrameMagic[:])
	header[4] = darwinProtocolVersion
	header[5] = frame.typeID
	binary.BigEndian.PutUint32(header[8:12], uint32(len(method)))
	binary.BigEndian.PutUint32(header[12:16], uint32(len(frame.payload)))
	for _, value := range [][]byte{header[:], method, frame.payload} {
		for len(value) > 0 {
			written, errWrite := writer.Write(value)
			if errWrite != nil {
				return errWrite
			}
			if written <= 0 {
				return io.ErrShortWrite
			}
			value = value[written:]
		}
	}
	return nil
}

func darwinStatusPayload(status int32, body []byte) []byte {
	payload := make([]byte, len(body)+4)
	binary.BigEndian.PutUint32(payload[0:4], uint32(status))
	copy(payload[4:], body)
	return payload
}

func parseDarwinStatusPayload(payload []byte) (int32, []byte, error) {
	if len(payload) < 4 {
		return 0, nil, fmt.Errorf("macOS plugin response status is missing")
	}
	return int32(binary.BigEndian.Uint32(payload[0:4])), append([]byte(nil), payload[4:]...), nil
}
