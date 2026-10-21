#!/bin/sh
# build-quickjs.sh — rebuild assets/quickjs.wasm, the run_code sandbox guest.
#
# Source provenance: quickjs-ng v0.15.1 as vendored inside the rquickjs-sys
# 0.12.2 crate (`cargo fetch` of that crate gives you the exact tree; GitHub
# clones/amalgam archives of the same tag are byte-equivalent for these four
# compilation units). Cross-compiler: `zig cc` (target wasm32-wasi, reactor
# model — no crt main). No wasi-sdk required.
#
# The module deliberately has ZERO wasi imports: quickjs-libc is NOT compiled,
# and the libc symbols the core still references are stubbed in the glue (see
# the "capability close-off" block there), so `env.host_call_tool` /
# `env.host_log` are the guest's entire capability surface.
#
# Current asset (verify after rebuilding — assets/quickjs.wasm):
#   sha256 3c1c87903f78d5180feebe06e425724b3bd958aedf39dc1d78878e8c7e7cd35a
#   size   1,001,851 bytes
#
# Usage: QJS=<dir with quickjs-ng sources> sh examples/build-quickjs.sh
set -eu

QJS="${QJS:?set QJS to the quickjs-ng v0.15.1 source dir (see header)}"
OUT="${1:-crates/core/assets/quickjs.wasm}"
GLUE="$(dirname "$0")/quickjs_glue.c"

# -mexec-model=reactor  : no crt main; ctors run via the exported _initialize
# --no-entry            : same, linker side
# --export=…            : the host ABI (design doc run_code_design.md §5.1)
# -z stack-size         : wasm linear-memory stack reservation (1 MiB)
# --initial-memory      : 32 MiB — the host's mem_mb floor is this value
# --strip-all           : drops C symbols/debug info (4.7 MB → ~1 MB)
zig cc --target=wasm32-wasi -mexec-model=reactor -O2 -DNDEBUG -D_GNU_SOURCE \
  -Wl,--no-entry -Wl,--export=alloc -Wl,--export=dealloc \
  -Wl,--export=eval_code -Wl,--export=take_result -Wl,--export=take_last_error \
  -Wl,-z,stack-size=1048576 -Wl,--initial-memory=33554432 -Wl,--strip-all \
  -I"$QJS" "$GLUE" \
  "$QJS/quickjs.c" "$QJS/dtoa.c" "$QJS/libregexp.c" "$QJS/libunicode.c" \
  -o "$OUT"

echo "built $OUT"
shasum -a 256 "$OUT"
# Sanity: the module must import exactly env.host_log + env.host_call_tool.
# (Any wasi import leaking in means a new libc symbol got pulled — extend the
# glue's stub block, don't ship it.)
if command -v wasm-objdump >/dev/null 2>&1; then
  wasm-objdump -x "$OUT" | grep -E "^ *import" || true
fi
