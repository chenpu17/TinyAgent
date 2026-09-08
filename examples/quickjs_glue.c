// glue.c — TinyAgent run_code guest: quickjs-ng bound to the host ABI.
//
// Compiled to wasm32-wasi WITHOUT quickjs-libc, so the module has no WASI
// imports at all (no fd/clock/env/args): the only imports are
// env.host_call_tool / env.host_log, which is the guest's entire capability
// surface (docs/run_code_design.md §5). The JS-visible prelude —
// globalThis.tools.{call,list}, console.log, and the Date.now / Math.random
// poison — is implemented here as native bindings, so the import wrappers are
// never reachable from JS (capability close-off, design §5.3).
//
// Exports (see design §5.1):
//   alloc(size) -> ptr                 malloc wrapper (host writes code/args)
//   dealloc(ptr)                       free wrapper
//   eval_code(code_ptr, code_len) -> 0 (ok) | 1 (JS exception)
//   take_result()      -> i64 (ptr << 32) | len   JSON of the return value
//   take_last_error()  -> i64 (ptr << 32) | len   message + "\n" + stack
//
// One eval per instance (the host drops the Store after reading the result),
// so the result slots are plain statics — no cross-eval lifetime to manage.

#include <stdbool.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>

// wasi-musl declares these as macros (off_t-width dispatch) — define plain
// functions instead, which is what wins the archive resolution below.
#undef lseek
#undef fstat

#include "quickjs.h"

#define QJS_VERSION_STRING "0.15.1"

// ── capability close-off stubs ─────────────────────────────────────────────
// quickjs core still references a few libc symbols whose wasi-libc versions
// would import WASI (fd_write / fd_seek / fd_close / fd_fdstat_get /
// clock_time_get). Defining them here wins archive resolution, so the linked
// module keeps ZERO wasi imports — the sandbox stays constructive: no
// stubbed-out ambient call can ever observe the host, they just swallow.
// (Date.now / Math.random are additionally poisoned at the JS level, FR-2.)
static void ta_swallow(const void *p, size_t n) { (void)p; (void)n; }

ssize_t write(int fd, const void *buf, size_t n) {
    (void)fd;
    ta_swallow(buf, n);
    return (ssize_t)n; // pretend the bytes went somewhere
}

int close(int fd) { (void)fd; return 0; }
off_t lseek(int fd, off_t off, int whence) { (void)fd; (void)off; (void)whence; return (off_t)-1; }
int fstat(int fd, struct stat *st) { (void)fd; memset(st, 0, sizeof(*st)); return -1; }
int isatty(int fd) { (void)fd; return 0; }
int clock_gettime(clockid_t clk, struct timespec *tp) {
    (void)clk;
    tp->tv_sec = 0;
    tp->tv_nsec = 0; // deterministic: the guest has no clock, FR-2
    return 0;
}

// The remaining stragglers come through libc internals (stdio seeks its
// buffers, …) that don't route through the overridable posix wrappers, so
// define the WASI-preview1 surface itself (declared in <wasi/api.h>, which
// the libc headers pull in). Every one of these sits on a code path that is
// unreachable from the sandbox; they exist only so the linked module has
// ZERO wasi imports. If wasm-ld reports a duplicate, the corresponding libc
// object got pulled for a sibling symbol — extend this block, don't delete.
__wasi_errno_t __wasi_fd_write(__wasi_fd_t fd, const __wasi_ciovec_t *iovs,
                               size_t iovs_len, size_t *nwritten) {
    (void)fd;
    (void)iovs;
    (void)iovs_len;
    *nwritten = 0;
    return 0;
}

__wasi_errno_t __wasi_fd_seek(__wasi_fd_t fd, __wasi_filedelta_t offset,
                              __wasi_whence_t whence, __wasi_filesize_t *newoffset) {
    (void)fd;
    (void)offset;
    (void)whence;
    *newoffset = 0;
    return 0;
}

__wasi_errno_t __wasi_fd_fdstat_get(__wasi_fd_t fd, __wasi_fdstat_t *buf) {
    (void)fd;
    memset(buf, 0, sizeof(*buf));
    return 0;
}

__wasi_errno_t __wasi_clock_time_get(__wasi_clockid_t clock_id,
                                     __wasi_timestamp_t precision,
                                     __wasi_timestamp_t *time) {
    (void)clock_id;
    (void)precision;
    *time = 0; // deterministic: the guest has no clock, FR-2
    return 0;
}

// ── host imports ───────────────────────────────────────────────────────────

__attribute__((import_module("env"), import_name("host_call_tool")))
int64_t host_call_tool(const uint8_t *name, int32_t name_len,
                       const uint8_t *args, int32_t args_len);

__attribute__((import_module("env"), import_name("host_log")))
void host_log(const uint8_t *msg, int32_t msg_len);

// ── result slots (live until the next eval) ────────────────────────────────

static uint8_t *g_result = NULL, *g_error = NULL;
static int32_t g_result_len = 0, g_error_len = 0;

// JS_SetMaxStackSize is INERT on wasm: quickjs-ng's update_stack_limit()
// hard-disables the stack limit under __wasi__ (linear-memory stack), so the
// recursion bound is whatever the HOST engine enforces — wasmtime's
// max_wasm_stack (384 KiB as of M3 → "call stack exhausted" trap; the
// earlier 1.5 MiB lost a measured race against quickjs's own unchecked
// internal recursion state — see run_code_design.md §6 four-source note)
// or V8's stack guard. The call stays for source parity (and in case
// quickjs-ng ever enables wasi limits). Deep recursion therefore terminates
// the eval; it is NOT a catchable JS RangeError inside the guest.
#define TA_STACK_LIMIT (64 * 1024)

// Store two byte-ranges as one buffer with a '\n' between them (either may
// be empty). Used for result copies and the message+stack error format.
static void set_buf(uint8_t **slot, int32_t *len_slot, const void *a, int32_t alen,
                    const void *b, int32_t blen) {
    free(*slot);
    int32_t sep = (alen > 0 && blen > 0) ? 1 : 0;
    *slot = malloc((size_t)alen + (size_t)sep + (size_t)blen + 1);
    if (alen > 0 && a) memcpy(*slot, a, (size_t)alen);
    if (sep) (*slot)[alen] = '\n';
    if (blen > 0 && b) memcpy(*slot + alen + sep, b, (size_t)blen);
    (*slot)[alen + sep + blen] = 0;
    *len_slot = alen + sep + blen;
}

// ── exports: memory plumbing ───────────────────────────────────────────────

int32_t alloc(int32_t size) { return (int32_t)(uintptr_t)malloc((size_t)size); }

void dealloc(int32_t ptr) { free((void *)(uintptr_t)ptr); }

// ── envelope handling (design §5.1 dual envelope) ──────────────────────────

// Frees the host-provided buffer, then:
//   {"__ok":true,"content":S}  → returns S parsed as JSON when possible,
//                               otherwise the raw string (D4)
//   {"__ok":false,"error":S}   → throws a plain Error carrying S
//
// JS_ParseJSON's contract requires buf[buf_len] == '\0' (its tokenizer scans
// past buf_len for the EOF token) — the host hands us exact-length buffers,
// so parse from a fresh NUL-terminated copy, never in place.
static JSValue consume_envelope(JSContext *ctx, uint8_t *ptr, int32_t len) {
    char *zterm = malloc((size_t)len + 1);
    if (!zterm) {
        dealloc((int32_t)(uintptr_t)ptr);
        return JS_ThrowTypeError(ctx, "run_code: out of memory");
    }
    // NOTE(sec-review n-4, won't-fix): on the already-trapped fallback path
    // (host double-write failed → ptr==NULL, len==0) this is memcpy(dst,
    // NULL, 0) — technically UB per the letter of the standard, benign on
    // every real toolchain, and only reachable when the run is already
    // dying. Guarding it here would drift this source from the checked-in
    // prebuilt asset's sha256; rebuilds pick the guard up if ever wanted.
    memcpy(zterm, ptr, (size_t)len);
    zterm[len] = 0;
    dealloc((int32_t)(uintptr_t)ptr); // host-malloc'd; ours to free after copy
    JSValue env = JS_ParseJSON(ctx, zterm, (size_t)len, "<envelope>");
    free(zterm);
    if (JS_IsException(env))
        return JS_ThrowTypeError(ctx, "run_code: malformed host envelope");

    JSValue ok = JS_GetPropertyStr(ctx, env, "__ok");
    int is_ok = JS_ToBool(ctx, ok);
    JS_FreeValue(ctx, ok);

    JSValue ret;
    if (is_ok > 0) {
        JSValue content = JS_GetPropertyStr(ctx, env, "content");
        if (JS_IsString(content)) {
            size_t clen;
            const char *cs = JS_ToCStringLen(ctx, &clen, content);
            JS_FreeValue(ctx, content);
            JSValue parsed = JS_ParseJSON(ctx, cs, clen, "<tool-output>");
            if (!JS_IsException(parsed)) {
                ret = parsed; // parseable → object/array/number/...
            } else {
                JS_FreeValue(ctx, parsed);
                ret = JS_NewStringLen(ctx, cs, clen); // D4: raw string
            }
            JS_FreeCString(ctx, cs);
        } else {
            ret = content; // non-string content (null/number/…) → as-is
        }
    } else {
        JSValue errv = JS_GetPropertyStr(ctx, env, "error");
        size_t elen = 0;
        const char *es = JS_ToCStringLen(ctx, &elen, errv);
        JS_FreeValue(ctx, errv);
        if (!es) {
            es = "tool call failed";
            elen = strlen(es);
        }
        // plain Error, so guest `catch (e) { e.message }` sees the host message
        JSValue ctor = JS_GetPropertyStr(ctx, JS_GetGlobalObject(ctx), "Error");
        JSValue msg = JS_NewStringLen(ctx, es, elen);
        JSValue err = JS_Call(ctx, ctor, JS_UNDEFINED, 1, &msg);
        JS_FreeValue(ctx, msg);
        JS_FreeValue(ctx, ctor);
        JS_FreeCString(ctx, es);
        // Structured error fields (v2): the host classifies what it knows
        // ("kind", optional "tool" / "retryable") — attach them to the
        // thrown Error so batch code branches on e.kind instead of parsing
        // e.message. Absent envelope fields leave the property undefined.
        if (!JS_IsException(err) && !JS_IsNull(err) && !JS_IsUndefined(err)) {
            static const char *const kFields[] = {"kind", "tool", "retryable"};
            for (size_t i = 0; i < sizeof(kFields) / sizeof(kFields[0]); i++) {
                JSValue v = JS_GetPropertyStr(ctx, env, kFields[i]);
                if (JS_IsUndefined(v) || JS_IsNull(v) || JS_IsException(v)) {
                    JS_FreeValue(ctx, v);
                    continue;
                }
                if (JS_DefinePropertyValueStr(ctx, err, kFields[i], v,
                                              JS_PROP_C_W_E) < 0)
                    JS_FreeValue(ctx, v); // define failed — don't leak
            }
        }
        ret = JS_IsException(err) ? err : JS_Throw(ctx, err);
    }
    JS_FreeValue(ctx, env);
    return ret;
}

// Shared by tools.call and tools.list: bytes in, envelope out.
static JSValue call_host(JSContext *ctx, const char *name, size_t name_len,
                         const char *args_json, size_t args_len) {
    int64_t packed = host_call_tool((const uint8_t *)name, (int32_t)name_len,
                                    (const uint8_t *)args_json, (int32_t)args_len);
    uint8_t *ptr = (uint8_t *)(uintptr_t)(uint32_t)(packed >> 32);
    int32_t len = (int32_t)(uint32_t)packed;
    return consume_envelope(ctx, ptr, len);
}

// ── JS-visible prelude bindings ────────────────────────────────────────────

static JSValue js_tools_call(JSContext *ctx, JSValueConst this_val, int argc,
                             JSValueConst *argv) {
    (void)this_val;
    if (argc < 1 || !JS_IsString(argv[0]))
        return JS_ThrowTypeError(ctx, "tools.call(name, args): name must be a string");

    const char *name = JS_ToCString(ctx, argv[0]);
    if (!name)
        return JS_EXCEPTION;

    JSValue args = argc > 1 ? JS_JSONStringify(ctx, argv[1], JS_UNDEFINED, JS_UNDEFINED)
                            : JS_UNDEFINED;
    if (JS_IsException(args)) {
        JS_FreeCString(ctx, name);
        return args; // e.g. a cyclic object: propagate the TypeError
    }

    JSValue ret;
    if (JS_IsUndefined(args)) {
        // JSON.stringify(undefined) is undefined — treat like "no args"
        ret = call_host(ctx, name, strlen(name), "{}", 2);
    } else {
        const char *args_json = JS_ToCString(ctx, args);
        ret = call_host(ctx, name, strlen(name), args_json, strlen(args_json));
        JS_FreeCString(ctx, args_json);
    }
    JS_FreeValue(ctx, args);
    JS_FreeCString(ctx, name);
    return ret;
}

static JSValue js_tools_list(JSContext *ctx, JSValueConst this_val, int argc,
                             JSValueConst *argv) {
    (void)this_val;
    (void)argc;
    (void)argv;
    return call_host(ctx, "__tools_list", 12, "{}", 2);
}

// Growable line buffer for console output.
typedef struct {
    char *buf;
    size_t len, cap;
} sbuf;

static void sbuf_push(sbuf *s, const char *data, size_t n) {
    if (s->len + n + 1 > s->cap) {
        while (s->len + n + 1 > s->cap) s->cap = s->cap ? s->cap * 2 : 64;
        s->buf = realloc(s->buf, s->cap);
    }
    memcpy(s->buf + s->len, data, n);
    s->len += n;
    s->buf[s->len] = 0;
}

static void sbuf_push_value(JSContext *ctx, sbuf *s, JSValueConst v) {
    const char *cs = NULL;
    size_t clen;
    if (JS_IsString(v)) {
        cs = JS_ToCStringLen(ctx, &clen, v);
    } else {
        JSValue j = JS_JSONStringify(ctx, v, JS_UNDEFINED, JS_UNDEFINED);
        if (JS_IsException(j)) {
            JS_FreeValue(ctx, j);
            JSValue t = JS_ToString(ctx, v); // e.g. a cyclic object
            cs = JS_ToCStringLen(ctx, &clen, t);
            JS_FreeValue(ctx, t);
        } else if (JS_IsUndefined(j)) {
            JS_FreeValue(ctx, j);
            JSValue t = JS_ToString(ctx, v); // undefined, symbol, function
            cs = JS_ToCStringLen(ctx, &clen, t);
            JS_FreeValue(ctx, t);
        } else {
            cs = JS_ToCStringLen(ctx, &clen, j);
            JS_FreeValue(ctx, j);
        }
    }
    if (cs) {
        sbuf_push(s, cs, clen);
        JS_FreeCString(ctx, cs);
    }
    // else: string conversion threw — the exception stays pending and
    // surfaces from console.log itself; acceptable for a debug channel.
}

static JSValue js_console_log(JSContext *ctx, JSValueConst this_val, int argc,
                              JSValueConst *argv) {
    (void)this_val;
    sbuf s = {0};
    for (int i = 0; i < argc; i++) {
        if (i)
            sbuf_push(&s, " ", 1);
        sbuf_push_value(ctx, &s, argv[i]);
    }
    host_log((const uint8_t *)(s.buf ? s.buf : ""), (int32_t)s.len);
    free(s.buf);
    return JS_UNDEFINED;
}

// Date.now / Math.random poison: accidental nondeterminism must be loud, not
// silent (FR-2 determinism). A guest re-defining its own shim only gets its
// own deterministic code — no capability is gained.
static const char POISON[] =
    "for (const [o, k] of [[Date, 'now'], [Math, 'random']])"
    "  Object.defineProperty(o, k, {configurable: true, get() {"
    "    throw new TypeError(k + '() is not available in run_code"
    " (deterministic sandbox)');"
    "  }});";

static void register_bindings(JSContext *ctx) {
    JSValue global = JS_GetGlobalObject(ctx);

    JSValue tools = JS_NewObject(ctx);
    JS_SetPropertyStr(ctx, tools, "call", JS_NewCFunction(ctx, js_tools_call, "call", 2));
    JS_SetPropertyStr(ctx, tools, "list", JS_NewCFunction(ctx, js_tools_list, "list", 0));
    JS_SetPropertyStr(ctx, global, "tools", tools);

    JSValue console = JS_NewObject(ctx);
    JSValue log_fn = JS_NewCFunction(ctx, js_console_log, "log", 0);
    JS_SetPropertyStr(ctx, console, "log", log_fn);
    JS_SetPropertyStr(ctx, console, "info", JS_DupValue(ctx, log_fn));
    JS_SetPropertyStr(ctx, console, "warn", JS_DupValue(ctx, log_fn));
    JS_SetPropertyStr(ctx, console, "error", JS_DupValue(ctx, log_fn));
    JS_SetPropertyStr(ctx, global, "console", console);

    JS_FreeValue(ctx, global);

    JSValue r = JS_Eval(ctx, POISON, sizeof(POISON) - 1, "<poison>", JS_EVAL_TYPE_GLOBAL);
    JS_FreeValue(ctx, r); // must not fail; ignore the result either way
}

// ── exports: evaluation ────────────────────────────────────────────────────

int32_t eval_code(const uint8_t *code, int32_t code_len) {
    JSRuntime *rt = JS_NewRuntime();
    if (!rt) {
        static const char m[] = "out of memory: cannot create JS runtime";
        set_buf(&g_error, &g_error_len, m, sizeof(m) - 1, NULL, 0);
        return 1;
    }
    JS_SetMaxStackSize(rt, TA_STACK_LIMIT);
    JSContext *ctx = JS_NewContext(rt);
    register_bindings(ctx);

    // Wrap as a function body so top-level `return` works (the contract the
    // LLM-visible description promises).
    static const char PREFIX[] = "(function(){";
    static const char SUFFIX[] = "\n})()";
    size_t wrapped_len = sizeof(PREFIX) - 1 + (size_t)code_len + sizeof(SUFFIX) - 1;
    char *wrapped = malloc(wrapped_len + 1);
    if (!wrapped) {
        static const char m[] = "out of memory";
        set_buf(&g_error, &g_error_len, m, sizeof(m) - 1, NULL, 0);
        JS_FreeContext(ctx);
        JS_FreeRuntime(rt);
        return 1;
    }
    memcpy(wrapped, PREFIX, sizeof(PREFIX) - 1);
    memcpy(wrapped + sizeof(PREFIX) - 1, code, (size_t)code_len);
    memcpy(wrapped + sizeof(PREFIX) - 1 + (size_t)code_len, SUFFIX, sizeof(SUFFIX) - 1);
    wrapped[wrapped_len] = 0;

    JSValue r = JS_Eval(ctx, wrapped, wrapped_len, "<run_code>", JS_EVAL_TYPE_GLOBAL);
    free(wrapped);

    if (JS_IsException(r)) {
        JSValue exc = JS_GetException(ctx);
        const char *msg = JS_ToCString(ctx, exc);
        if (!msg)
            msg = "<unserializable exception>";
        JSValue stackv = JS_GetPropertyStr(ctx, exc, "stack");
        const char *stack = JS_ToCString(ctx, stackv);
        set_buf(&g_error, &g_error_len, msg, (int32_t)strlen(msg),
                stack ? stack : "", stack ? (int32_t)strlen(stack) : 0);
        if (stack)
            JS_FreeCString(ctx, stack);
        if (msg)
            JS_FreeCString(ctx, msg);
        JS_FreeValue(ctx, stackv);
        JS_FreeValue(ctx, exc);
        JS_FreeValue(ctx, r);
        JS_FreeContext(ctx);
        JS_FreeRuntime(rt);
        return 1;
    }

    // Serialize the return value; anything JSON.stringify refuses falls back
    // to ToString, and a void/undefined return becomes the literal "undefined".
    JSValue j = JS_JSONStringify(ctx, r, JS_UNDEFINED, JS_UNDEFINED);
    const char *out = NULL;
    size_t out_len = 0;
    bool out_owned = false;
    if (JS_IsException(j)) {
        JS_FreeValue(ctx, j);
        JSValue t = JS_ToString(ctx, r);
        out = JS_ToCStringLen(ctx, &out_len, t);
        out_owned = true;
        JS_FreeValue(ctx, t);
    } else if (JS_IsUndefined(j)) {
        out = "undefined";
        out_len = 10;
        JS_FreeValue(ctx, j);
    } else {
        out = JS_ToCStringLen(ctx, &out_len, j);
        out_owned = true;
        JS_FreeValue(ctx, j);
    }
    set_buf(&g_result, &g_result_len, out, (int32_t)out_len, NULL, 0);
    if (out_owned)
        JS_FreeCString(ctx, out);

    JS_FreeValue(ctx, r);
    JS_FreeContext(ctx);
    JS_FreeRuntime(rt);
    return 0;
}

int64_t take_result(void) {
    return ((int64_t)(uint32_t)(uintptr_t)g_result << 32) | (uint32_t)g_result_len;
}

int64_t take_last_error(void) {
    return ((int64_t)(uint32_t)(uintptr_t)g_error << 32) | (uint32_t)g_error_len;
}
