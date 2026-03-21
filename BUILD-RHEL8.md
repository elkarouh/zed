# Building Zed on RHEL 8

## Prerequisites

GCC 8.5 (system default) cannot compile the WebRTC C++20 code. Use GCC Toolset 13:

```bash
source /opt/rh/gcc-toolset-13/enable
```

A custom `xkbcommon-x11.pc` is needed at `~/.local/lib/pkgconfig/` and a linker
symlink at `~/.local/lib64/libxkbcommon-x11.so`.

## Environment

```bash
source /opt/rh/gcc-toolset-13/enable
export PKG_CONFIG_PATH="$HOME/.local/lib/pkgconfig:${PKG_CONFIG_PATH:-}"
export LIBRARY_PATH="$HOME/.local/lib64:${LIBRARY_PATH:-}"
```

## Build

```bash
cd ~/Downloads/zed-main
cargo build -p zed
```

The binary is produced at `target/debug/zed`.

## Install

A symlink at `~/bin/zed` points to the built binary:

```bash
ln -sf "$PWD/target/debug/zed" ~/bin/zed
```

## Wrapper scripts

`~/bin/zd` -- launches Zed with proxy setup and quiet logging (`ZED_LOG=error`).

`~/bin/hzd` -- same as `zd` but forces software Vulkan rendering (LavaPipe) via
`VK_ICD_FILENAMES` and `WGPU_BACKEND=vulkan`. Useful when the hardware GPU driver
is unavailable or broken.

## Local patches

- `crates/gpui/src/window.rs`: Changed `.log_err()` to `.warn_on_err()` on all
  `handle.update()` calls in window event callbacks (lines 1210-1372). The
  "window not found" message during startup is a benign race condition and should
  not be logged at ERROR level.
