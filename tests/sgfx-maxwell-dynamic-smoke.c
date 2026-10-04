/* Executed by the real Scarlet interpreter, without libc or Linux syscalls. */
#include <sgfx_backend.h>

extern void *dlopen(const char *, int);
extern void *dlsym(void *, const char *);
extern int dlclose(void *);
extern char *dlerror(void);

/* Optional extension; scalar layout matches the Maxwell plugin's v2 table. */
typedef struct { uint32_t size, reserved, matrix, range, chroma_x, chroma_y; } ycbcr_conversion;
typedef struct {
    uint32_t version, size;
    int32_t (*import_ycbcr)(sgfx_object, uint32_t, int32_t, ycbcr_conversion);
} ycbcr_api;
typedef int32_t (*get_ycbcr_api)(uint32_t, size_t, ycbcr_api *);
_Static_assert(sizeof(ycbcr_conversion) == 24, "YCbCr conversion layout");
_Static_assert(sizeof(ycbcr_api) == 16, "YCbCr extension layout");

static uintptr_t native_call(uintptr_t number, uintptr_t value) {
    register uintptr_t x0 __asm__("x0") = value;
    register uintptr_t x8 __asm__("x8") = number;
    __asm__ volatile("svc #0" : "+r"(x0) : "r"(x8) : "memory");
    return x0;
}

static void print(const char *message) {
    while (*message) native_call(16, (unsigned char)*message++);
}

__attribute__((noreturn)) static void fail(const char *reason) {
    print("SCARLET_MAXWELL_DYNAMIC_FAIL: ");
    print(reason);
    print("\n");
    for (;;) native_call(21, 0);
}

static int matches(const uint8_t *actual, const char *expected, size_t size) {
    size_t index = 0;
    while (index < size && expected[index]) {
        if (actual[index] != (uint8_t)expected[index]) return 0;
        index++;
    }
    return index < size && actual[index] == 0 && expected[index] == 0;
}

__attribute__((noreturn)) void _start(uintptr_t argc, char **argv) {
    if (!argc || !argv || !argv[0] || argv[argc]) fail("native startup arguments");
    print("SCARLET_MAXWELL_DYNAMIC_START\n");
    void *library = dlopen("/system/lib/sgfx/libsgfx_scarlet_maxwell.so", 0x102);
    if (!library) {
        char *error = dlerror();
        fail(error ? error : "dlopen actual Maxwell driver");
    }
    print("SCARLET_MAXWELL_DYNAMIC_DLOPEN_OK\n");
    sgfx_get_api get_api = (sgfx_get_api)dlsym(library, SGFX_BACKEND_ENTRY);
    sgfx_get_driver_api get_driver = (sgfx_get_driver_api)dlsym(library, SGFX_DRIVER_ENTRY);
    if (!get_api || !get_driver) fail("required entry lookup");
    sgfx_host_info host = { sizeof(host), 0, 0 }; /* Cortex-A57 has no LSE. */
    sgfx_backend_api api = {0};
    api.version = 0xa5a5a5a5;
    if (get_api(1, sizeof(api), &host, &api) != SGFX_ABI_MISMATCH || api.version != 0xa5a5a5a5)
        fail("base negotiation version rejection");
    if (get_api(2, sizeof(api) - 1, &host, &api) != SGFX_ABI_MISMATCH || api.version != 0xa5a5a5a5)
        fail("base negotiation short table rejection");
    if (get_api(2, sizeof(api), &host, 0) != SGFX_INVALID)
        fail("base negotiation null output rejection");
    if (get_api(2, sizeof(api), &host, &api) != SGFX_OK || api.version != 2 || api.size != sizeof(api))
        fail("base negotiation");
    if (!matches(api.name, "scarlet-maxwell", sizeof(api.name)) ||
        !matches(api.gpu_backend, "nvidia-gm20b", sizeof(api.gpu_backend)))
        fail("driver identity");
    sgfx_driver_api driver = {0};
    driver.version = 0xa5a5a5a5;
    if (get_driver(1, sizeof(driver), &driver) != SGFX_ABI_MISMATCH || driver.version != 0xa5a5a5a5)
        fail("driver negotiation version rejection");
    if (get_driver(2, sizeof(driver), &driver) != SGFX_OK || driver.version != 2 || driver.size != sizeof(driver))
        fail("driver negotiation");
    print("SCARLET_MAXWELL_DYNAMIC_TABLES_OK\n");

    get_ycbcr_api get_ycbcr = (get_ycbcr_api)dlsym(library, "sgfx_backend_get_ycbcr_api_v2");
    if (get_ycbcr) {
        ycbcr_api ycbcr = {0};
        if (get_ycbcr(2, sizeof(ycbcr), &ycbcr) != SGFX_OK || ycbcr.version != 2 || ycbcr.size != sizeof(ycbcr))
            fail("YCbCr negotiation");
        ycbcr_conversion conversion = {24, 0, 1, 1, 1, 1};
        if (ycbcr.import_ycbcr(0, 0, -1, conversion) != SGFX_INVALID)
            fail("YCbCr invalid owned handle rejection");
        print("SCARLET_MAXWELL_DYNAMIC_YCBCR_OK\n");
    } else {
        if (!dlerror()) fail("missing optional entry error");
#if SCARLET_REQUIRE_YCBCR
        fail("audited YCbCr entry not found");
#else
        print("SCARLET_MAXWELL_DYNAMIC_YCBCR_ABSENT\n");
#endif
    }

    /* Exercise the library's own native std path and verify failure outputs. */
    const uint8_t missing[] = "/dev/sgfx-smoke-missing-gpu";
    sgfx_bytes path = { missing, sizeof(missing) - 1 };
    for (unsigned attempt = 0; attempt < 16; attempt++) {
        sgfx_object device = (sgfx_object)0x1000;
        uint64_t caps = UINT64_MAX;
        if (api.open(path, &device, &caps) != SGFX_INITIALIZATION_FAILED || device || caps)
            fail("missing GPU rejection or initialized outputs");
    }
    print("SCARLET_MAXWELL_DYNAMIC_NATIVE_STD_OK\n");
    sgfx_object object = (sgfx_object)0x1000;
    uint64_t caps = UINT64_MAX;
    if (api.open(path, &object, 0) != SGFX_INVALID || object)
        fail("open null output boundary");
    object = (sgfx_object)0x1000;
    if (api.create_context(0, &object) != SGFX_INVALID || object)
        fail("context null device boundary");
    sgfx_words empty = {0, 0};
    sgfx_slots targets = {0, 0};
    object = (sgfx_object)0x1000;
    if (api.create_session(0, empty, targets, &object, &caps) != SGFX_INVALID || object || caps)
        fail("session null context boundary");
    object = (sgfx_object)0x1000;
    if (driver.create_resources(0, empty, &object) != SGFX_INVALID || object)
        fail("resources null context boundary");
    sgfx_image_info image = {1, 2, 3, 4};
    object = (sgfx_object)0x1000;
    if (driver.create_image(0, 1, 1, &object, &image) != SGFX_INVALID || object ||
        image.width || image.height || image.handle != -1 || image.reserved)
        fail("image null context initialized outputs");
    sgfx_submit_result submission = {SGFX_ACCEPTED, SGFX_OK, (sgfx_object)0x1000};
    api.submit(0, 0, &submission);
    if (submission.disposition != SGFX_REJECTED || submission.error != SGFX_INVALID || submission.receipt)
        fail("session submit rejection outputs");
    submission.disposition = SGFX_ACCEPTED;
    submission.error = SGFX_OK;
    submission.receipt = (sgfx_object)0x1000;
    driver.submit(0, 0, 0, &submission);
    if (submission.disposition != SGFX_REJECTED || submission.error != SGFX_INVALID || submission.receipt)
        fail("queue submit rejection outputs");
    uint32_t completion = SGFX_COMPLETE;
    if (api.wait(0, 0, &completion) != SGFX_INVALID || completion != SGFX_PENDING)
        fail("receipt wait rejection outputs");
    print("SCARLET_MAXWELL_DYNAMIC_BOUNDARIES_OK\n");
    if (dlclose(library)) fail("dlclose");
    print("SCARLET_MAXWELL_DYNAMIC_OK\n");
    for (;;) native_call(21, 0);
}
