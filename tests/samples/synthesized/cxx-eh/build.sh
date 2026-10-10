#!/bin/sh
# Rebuilds cxx-eh-x86_64 and cxx-eh-arm64 in a container holding clang and lld
# (Debian trixie: clang lld), with no Apple SDK. The C++ runtime the image
# calls is a stub dylib (stubs.c, libcxxstub.dylib), so the image imports
# `__cxa_throw` and the rest as a real one does: only the image's unwind and
# exception tables matter, and nothing here runs.
#
#   docker run --rm -v "$PWD":/w -w /w <image> sh build.sh
set -eu
for arch in x86_64 arm64; do
    clang -target "$arch-apple-macos11" -O1 -c stubs.c -o "stubs-$arch.o"
    ld64.lld-19 -arch "$arch" -platform_version macos 11.0 11.0 -dylib \
        -install_name /usr/lib/libcxxstub.dylib -o "libcxxstub-$arch.dylib" "stubs-$arch.o"
    clang++ -target "$arch-apple-macos11" -O1 -fno-rtti -c cxx.cpp -o "cxx-$arch.o"
    ld64.lld-19 -arch "$arch" -platform_version macos 11.0 11.0 -e _main \
        -o "cxx-eh-$arch" "cxx-$arch.o" "libcxxstub-$arch.dylib"
done
rm -f ./*.o ./*.dylib
