ENV = source /opt/rh/gcc-toolset-13/enable && \
	export PKG_CONFIG_PATH="$$HOME/.local/lib/pkgconfig:$${PKG_CONFIG_PATH:-}" && \
	export LIBRARY_PATH="$$HOME/.local/lib64:$${LIBRARY_PATH:-}"

.PHONY: build clean link

build:
	$(ENV) && cargo build -p zed

clean:
	cargo clean

link:
	ln -sf $(PWD)/target/debug/zed ~/bin/zed

all: build link
