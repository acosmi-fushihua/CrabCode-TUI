#include <dlfcn.h>
#include <fcntl.h>
#include <pthread.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#define CRABCODE_PROTOCOL_READ_FD 3
#define CRABCODE_PROTOCOL_WRITE_FD 4
#define CRABCODE_CALLBACK_WRITE_FD 5
#define CRABCODE_CALLBACK_READ_FD 6
#define CRABCODE_PROTOCOL_VERSION 1
#define CRABCODE_MAX_FIELD_BYTES (64U * 1024U * 1024U)

enum crabcode_frame_type {
	CRABCODE_FRAME_HELLO = 1,
	CRABCODE_FRAME_CALL = 2,
	CRABCODE_FRAME_CALL_RESPONSE = 3,
	CRABCODE_FRAME_HOST_CALL = 4,
	CRABCODE_FRAME_HOST_RESPONSE = 5,
	CRABCODE_FRAME_SHUTDOWN = 6,
	CRABCODE_FRAME_ERROR = 7,
};

typedef struct {
	void *ptr;
	size_t len;
} cliproxy_buffer;

typedef int (*cliproxy_host_call_fn)(
	void *,
	const char *,
	const uint8_t *,
	size_t,
	cliproxy_buffer *
);
typedef void (*cliproxy_host_free_fn)(void *, size_t);

typedef struct {
	uint32_t abi_version;
	void *host_ctx;
	cliproxy_host_call_fn call;
	cliproxy_host_free_fn free_buffer;
} cliproxy_host_api;

typedef int (*cliproxy_plugin_call_fn)(
	char *,
	uint8_t *,
	size_t,
	cliproxy_buffer *
);
typedef void (*cliproxy_plugin_free_fn)(void *, size_t);
typedef void (*cliproxy_plugin_shutdown_fn)(void);

typedef struct {
	uint32_t abi_version;
	cliproxy_plugin_call_fn call;
	cliproxy_plugin_free_fn free_buffer;
	cliproxy_plugin_shutdown_fn shutdown;
} cliproxy_plugin_api;

typedef int (*cliproxy_plugin_init_fn)(cliproxy_host_api *, cliproxy_plugin_api *);

typedef struct {
	uint8_t type;
	char *method;
	uint32_t method_len;
	uint8_t *payload;
	uint32_t payload_len;
} crabcode_frame;

static FILE *protocol_in;
static FILE *protocol_out;
static FILE *callback_in;
static FILE *callback_out;
static pthread_mutex_t callback_mutex = PTHREAD_MUTEX_INITIALIZER;

static uint32_t decode_u32(const uint8_t *value) {
	return ((uint32_t)value[0] << 24) |
		((uint32_t)value[1] << 16) |
		((uint32_t)value[2] << 8) |
		(uint32_t)value[3];
}

static void encode_u32(uint8_t *target, uint32_t value) {
	target[0] = (uint8_t)(value >> 24);
	target[1] = (uint8_t)(value >> 16);
	target[2] = (uint8_t)(value >> 8);
	target[3] = (uint8_t)value;
}

static int read_exact(FILE *input, void *target, size_t len) {
	uint8_t *cursor = target;
	while (len > 0) {
		size_t count = fread(cursor, 1, len, input);
		if (count == 0) {
			return 0;
		}
		cursor += count;
		len -= count;
	}
	return 1;
}

static int write_exact(FILE *output, const void *source, size_t len) {
	const uint8_t *cursor = source;
	while (len > 0) {
		size_t count = fwrite(cursor, 1, len, output);
		if (count == 0) {
			return 0;
		}
		cursor += count;
		len -= count;
	}
	return 1;
}

static int write_frame(
	FILE *output,
	uint8_t type,
	const char *method,
	uint32_t method_len,
	const uint8_t *payload,
	uint32_t payload_len
) {
	if (method_len > CRABCODE_MAX_FIELD_BYTES || payload_len > CRABCODE_MAX_FIELD_BYTES) {
		return 0;
	}
	uint8_t header[16] = {
		'C', 'C', 'P', 'H',
		CRABCODE_PROTOCOL_VERSION,
		type,
		0, 0,
		0, 0, 0, 0,
		0, 0, 0, 0,
	};
	encode_u32(header + 8, method_len);
	encode_u32(header + 12, payload_len);
	if (!write_exact(output, header, sizeof(header)) ||
		(method_len > 0 && !write_exact(output, method, method_len)) ||
		(payload_len > 0 && !write_exact(output, payload, payload_len))) {
		return 0;
	}
	return fflush(output) == 0;
}

static void free_frame(crabcode_frame *frame) {
	if (frame == NULL) {
		return;
	}
	free(frame->method);
	free(frame->payload);
	memset(frame, 0, sizeof(*frame));
}

static int read_frame(FILE *input, crabcode_frame *frame) {
	uint8_t header[16];
	memset(frame, 0, sizeof(*frame));
	if (!read_exact(input, header, sizeof(header))) {
		return 0;
	}
	if (memcmp(header, "CCPH", 4) != 0 ||
		header[4] != CRABCODE_PROTOCOL_VERSION ||
		header[6] != 0 || header[7] != 0) {
		return -1;
	}
	frame->type = header[5];
	frame->method_len = decode_u32(header + 8);
	frame->payload_len = decode_u32(header + 12);
	if (frame->method_len > CRABCODE_MAX_FIELD_BYTES ||
		frame->payload_len > CRABCODE_MAX_FIELD_BYTES) {
		return -1;
	}
	if (frame->method_len > 0) {
		frame->method = calloc((size_t)frame->method_len + 1, 1);
		if (frame->method == NULL || !read_exact(input, frame->method, frame->method_len)) {
			free_frame(frame);
			return -1;
		}
		if (memchr(frame->method, '\0', frame->method_len) != NULL) {
			free_frame(frame);
			return -1;
		}
	}
	if (frame->payload_len > 0) {
		frame->payload = malloc(frame->payload_len);
		if (frame->payload == NULL || !read_exact(input, frame->payload, frame->payload_len)) {
			free_frame(frame);
			return -1;
		}
	}
	return 1;
}

static void write_error(const char *message) {
	if (protocol_out == NULL || message == NULL) {
		return;
	}
	size_t len = strlen(message);
	if (len > CRABCODE_MAX_FIELD_BYTES) {
		len = CRABCODE_MAX_FIELD_BYTES;
	}
	(void)write_frame(
		protocol_out,
		CRABCODE_FRAME_ERROR,
		NULL,
		0,
		(const uint8_t *)message,
		(uint32_t)len
	);
}

static int helper_host_call(
	void *host_ctx,
	const char *method,
	const uint8_t *request,
	size_t request_len,
	cliproxy_buffer *response
) {
	(void)host_ctx;
	if (response != NULL) {
		response->ptr = NULL;
		response->len = 0;
	}
	if (method == NULL || request_len > CRABCODE_MAX_FIELD_BYTES ||
		(request == NULL && request_len != 0)) {
		return 1;
	}
	size_t method_len = strnlen(method, CRABCODE_MAX_FIELD_BYTES + 1U);
	if (method_len == 0 || method_len > CRABCODE_MAX_FIELD_BYTES) {
		return 1;
	}
	if (pthread_mutex_lock(&callback_mutex) != 0) {
		return 1;
	}
	int result = 1;
	if (!write_frame(
			callback_out,
			CRABCODE_FRAME_HOST_CALL,
			method,
			(uint32_t)method_len,
			request,
			(uint32_t)request_len
		)) {
		goto done;
	}

	crabcode_frame frame;
	int read_status = read_frame(callback_in, &frame);
	if (read_status != 1 || frame.type != CRABCODE_FRAME_HOST_RESPONSE ||
		frame.method_len != 0 || frame.payload_len < 4) {
		if (read_status == 1) {
			free_frame(&frame);
		}
		goto done;
	}
	int32_t status = (int32_t)decode_u32(frame.payload);
	uint32_t body_len = frame.payload_len - 4;
	if (response != NULL && body_len > 0) {
		response->ptr = malloc(body_len);
		if (response->ptr == NULL) {
			free_frame(&frame);
			goto done;
		}
		memcpy(response->ptr, frame.payload + 4, body_len);
		response->len = body_len;
	}
	free_frame(&frame);
	result = status;

done:
	(void)pthread_mutex_unlock(&callback_mutex);
	return result;
}

static void helper_host_free(void *ptr, size_t len) {
	(void)len;
	free(ptr);
}

static int write_call_response(int32_t status, const void *ptr, size_t len) {
	if (len > CRABCODE_MAX_FIELD_BYTES - 4U) {
		return 0;
	}
	uint8_t *payload = malloc(len + 4U);
	if (payload == NULL) {
		return 0;
	}
	encode_u32(payload, (uint32_t)status);
	if (ptr != NULL && len > 0) {
		memcpy(payload + 4, ptr, len);
	}
	int ok = write_frame(
		protocol_out,
		CRABCODE_FRAME_CALL_RESPONSE,
		NULL,
		0,
		payload,
		(uint32_t)(len + 4U)
	);
	free(payload);
	return ok;
}

int main(int argc, char **argv) {
	signal(SIGPIPE, SIG_IGN);
	if (argc != 3 || strcmp(argv[1], "--plugin") != 0 || argv[2][0] == '\0') {
		fprintf(stderr, "usage: oauthapi-plugin-host --plugin <path>\n");
		return 64;
	}
	int protocol_fds[] = {
		CRABCODE_PROTOCOL_READ_FD,
		CRABCODE_PROTOCOL_WRITE_FD,
		CRABCODE_CALLBACK_WRITE_FD,
		CRABCODE_CALLBACK_READ_FD,
	};
	for (size_t index = 0; index < sizeof(protocol_fds) / sizeof(protocol_fds[0]); index++) {
		int flags = fcntl(protocol_fds[index], F_GETFD);
		if (flags < 0 || fcntl(protocol_fds[index], F_SETFD, flags | FD_CLOEXEC) < 0) {
			fprintf(stderr, "protect plugin host protocol descriptors\n");
			return 70;
		}
	}
	protocol_in = fdopen(CRABCODE_PROTOCOL_READ_FD, "rb");
	protocol_out = fdopen(CRABCODE_PROTOCOL_WRITE_FD, "wb");
	callback_out = fdopen(CRABCODE_CALLBACK_WRITE_FD, "wb");
	callback_in = fdopen(CRABCODE_CALLBACK_READ_FD, "rb");
	if (protocol_in == NULL || protocol_out == NULL ||
		callback_in == NULL || callback_out == NULL) {
		fprintf(stderr, "open plugin host protocol descriptors\n");
		return 70;
	}
	setvbuf(protocol_in, NULL, _IONBF, 0);
	setvbuf(protocol_out, NULL, _IONBF, 0);
	setvbuf(callback_in, NULL, _IONBF, 0);
	setvbuf(callback_out, NULL, _IONBF, 0);

	void *library = dlopen(argv[2], RTLD_NOW | RTLD_LOCAL);
	if (library == NULL) {
		write_error(dlerror());
		return 65;
	}
	dlerror();
	cliproxy_plugin_init_fn initialize =
		(cliproxy_plugin_init_fn)dlsym(library, "cliproxy_plugin_init");
	const char *symbol_error = dlerror();
	if (symbol_error != NULL || initialize == NULL) {
		write_error(symbol_error == NULL ? "missing cliproxy_plugin_init" : symbol_error);
		return 65;
	}

	cliproxy_host_api host = {
		.abi_version = CRABCODE_PROTOCOL_VERSION,
		.host_ctx = NULL,
		.call = helper_host_call,
		.free_buffer = helper_host_free,
	};
	cliproxy_plugin_api plugin = {0};
	int init_status = initialize(&host, &plugin);
	if (init_status != 0) {
		char message[96];
		snprintf(message, sizeof(message), "cliproxy_plugin_init returned %d", init_status);
		write_error(message);
		return 65;
	}
	if (plugin.abi_version != CRABCODE_PROTOCOL_VERSION ||
		plugin.call == NULL || plugin.free_buffer == NULL) {
		write_error("plugin ABI version or function table is invalid");
		if (plugin.shutdown != NULL) {
			plugin.shutdown();
		}
		return 65;
	}
	uint8_t hello[4];
	encode_u32(hello, plugin.abi_version);
	if (!write_frame(protocol_out, CRABCODE_FRAME_HELLO, NULL, 0, hello, sizeof(hello))) {
		return 74;
	}

	int exit_status = 0;
	for (;;) {
		crabcode_frame frame;
		int read_status = read_frame(protocol_in, &frame);
		if (read_status == 0) {
			break;
		}
		if (read_status != 1) {
			write_error("invalid plugin host protocol frame");
			exit_status = 74;
			break;
		}
		if (frame.type == CRABCODE_FRAME_SHUTDOWN &&
			frame.method_len == 0 && frame.payload_len == 0) {
			free_frame(&frame);
			break;
		}
		if (frame.type != CRABCODE_FRAME_CALL || frame.method_len == 0) {
			free_frame(&frame);
			write_error("unexpected plugin host request frame");
			exit_status = 74;
			break;
		}

		cliproxy_buffer response = {0};
		int call_status = plugin.call(
			frame.method,
			frame.payload,
			frame.payload_len,
			&response
		);
		free_frame(&frame);
		if (response.ptr == NULL && response.len != 0) {
			write_error("plugin returned a non-zero response length with a null pointer");
			exit_status = 74;
			break;
		}
		if (!write_call_response(call_status, response.ptr, response.len)) {
			if (response.ptr != NULL) {
				plugin.free_buffer(response.ptr, response.len);
			}
			exit_status = 74;
			break;
		}
		if (response.ptr != NULL) {
			plugin.free_buffer(response.ptr, response.len);
		}
	}
	if (plugin.shutdown != NULL) {
		plugin.shutdown();
	}
	// A Go c-shared image is intentionally left mapped until process exit.
	(void)library;
	return exit_status;
}
