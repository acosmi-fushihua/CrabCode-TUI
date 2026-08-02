#include <fcntl.h>
#include <pthread.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

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

static cliproxy_host_api host;

typedef struct {
    char *marker_path;
} async_callback_work;

static int write_response(cliproxy_buffer *response, const char *value) {
    if (response == NULL || value == NULL) {
        return 1;
    }
    size_t len = strlen(value);
    response->ptr = malloc(len);
    if (response->ptr == NULL) {
        response->len = 0;
        return 1;
    }
    memcpy(response->ptr, value, len);
    response->len = len;
    return 0;
}

static void *run_async_callback(void *raw_work) {
    async_callback_work *work = raw_work;
    const struct timespec delay = {.tv_sec = 0, .tv_nsec = 50 * 1000 * 1000};
    (void)nanosleep(&delay, NULL);
    static const uint8_t log_request[] =
        "{\"level\":\"info\",\"message\":\"isolated async plugin callback\"}";
    cliproxy_buffer host_response = {0};
    int status = host.call(
        host.host_ctx,
        "host.log",
        log_request,
        sizeof(log_request) - 1U,
        &host_response
    );
    if (host_response.ptr != NULL) {
        host.free_buffer(host_response.ptr, host_response.len);
    }
    if (status == 0) {
        int marker = open(work->marker_path, O_WRONLY | O_CREAT | O_TRUNC, 0600);
        if (marker >= 0) {
            static const char value[] = "ok";
            ssize_t written = write(marker, value, sizeof(value) - 1U);
            (void)written;
            (void)close(marker);
        }
    }
    free(work->marker_path);
    free(work);
    return NULL;
}

static int plugin_call(
    char *method,
    uint8_t *request,
    size_t request_len,
    cliproxy_buffer *response
) {
    if (method == NULL || response == NULL) {
        return 1;
    }
    if (strcmp(method, "test.async_callback") == 0) {
        if (request == NULL || request_len == 0 || memchr(request, '\0', request_len) != NULL) {
            return 1;
        }
        async_callback_work *work = calloc(1, sizeof(*work));
        if (work == NULL) {
            return 1;
        }
        work->marker_path = calloc(request_len + 1U, 1);
        if (work->marker_path == NULL) {
            free(work);
            return 1;
        }
        memcpy(work->marker_path, request, request_len);
        pthread_t thread;
        if (pthread_create(&thread, NULL, run_async_callback, work) != 0) {
            free(work->marker_path);
            free(work);
            return 1;
        }
        (void)pthread_detach(thread);
        return write_response(response, "{\"started\":true}");
    }
    if (strcmp(method, "test.callback") != 0) {
        return 1;
    }
    static const uint8_t log_request[] =
        "{\"level\":\"info\",\"message\":\"isolated plugin callback\"}";
    cliproxy_buffer host_response = {0};
    int status = host.call(
        host.host_ctx,
        "host.log",
        log_request,
        sizeof(log_request) - 1U,
        &host_response
    );
    if (host_response.ptr != NULL) {
        host.free_buffer(host_response.ptr, host_response.len);
    }
    if (status != 0) {
        return status;
    }
    return write_response(response, "{\"ok\":true}");
}

static void plugin_free(void *ptr, size_t len) {
    (void)len;
    free(ptr);
}

__attribute__((visibility("default")))
int cliproxy_plugin_init(cliproxy_host_api *host_api, cliproxy_plugin_api *plugin_api) {
    if (host_api == NULL || plugin_api == NULL || host_api->abi_version != 1U ||
        host_api->call == NULL || host_api->free_buffer == NULL) {
        return 1;
    }
    host = *host_api;
    plugin_api->abi_version = 1U;
    plugin_api->call = plugin_call;
    plugin_api->free_buffer = plugin_free;
    plugin_api->shutdown = NULL;
    return 0;
}
