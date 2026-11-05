#!/usr/bin/env bash
# Regenerate the PTX of the ARGB→NV12 convert, its 10-bit kernels and the band hash from their CUDA
# source.
#
# The encoder ships PTX rather than a cubin so `libcuda`'s own JIT compiles it for whatever GPU
# is present, which is what keeps the NVENC path free of a runtime CUDA compiler. Two edits make
# the result as portable as the kernel itself: the ISA version and target are lowered to the
# oldest that carries these instructions, so every driver back to CUDA 5.5 and every
# NVENC-capable GPU accepts the module, and `__ldg`'s non-coherent load becomes a plain one,
# which is a cache hint rather than a semantic.
#
# nvcc rejects a host compiler newer than the one its release knew, so a second argument names
# the C++ compiler to hand it (`-ccbin`); the kernel has no host code, only nvcc's own headers
# need it.
#
#   scripts/build-ptx.sh [path-to-nvcc] [path-to-host-c++]
set -euo pipefail
here="$(dirname "$(readlink -f "$0")")"
src="${here}/../pixelflux/src/encoders/argb_to_nv12.cu"
out="${here}/../pixelflux/src/encoders/argb_to_nv12.ptx"
nvcc="${1:-nvcc}"
ccbin=()
[ $# -ge 2 ] && ccbin=(-ccbin "$2")
tmp="$(mktemp -d)"
trap 'rm -rf "${tmp}"' EXIT
"${nvcc}" "${ccbin[@]}" -ptx -arch=compute_52 -o "${tmp}/out.ptx" "${src}"
sed -e 's/^\.version .*/.version 3.1/' \
    -e 's/^\.target .*/.target sm_30/' \
    -e 's/ld\.global\.nc\.u8/ld.global.u8   /g' \
    "${tmp}/out.ptx" > "${out}"
echo "wrote ${out}"
