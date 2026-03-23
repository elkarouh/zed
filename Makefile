# Requires gcc-toolset-13 (RHEL 8) and a sibling alacritty repo:
#   ../alacritty/alacritty_terminal  (patched via .cargo/config.toml)
#   Fork: https://github.com/elkarouh/alacritty  branch: local-patches
# See BUILD-RHEL8.md for full setup instructions.

ENV = source /opt/rh/gcc-toolset-13/enable && \
	export PKG_CONFIG_PATH="$$HOME/.local/lib/pkgconfig:$${PKG_CONFIG_PATH:-}" && \
	export LIBRARY_PATH="$$HOME/.local/lib64:$${LIBRARY_PATH:-}"

.PHONY: build clean link all

build:
	$(ENV) && cargo build -p zed

clean:
	cargo clean

link:
	ln -sf $(PWD)/target/debug/zed ~/bin/zed

all: build link
