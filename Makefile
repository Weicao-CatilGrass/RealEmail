# RealEmail 构建入口
#
# build 使用 musl 目标做静态链接，产物不依赖系统 glibc，
# 可以直接拷贝到其他 Linux 机器运行。

TARGET ?= x86_64-unknown-linux-musl
BIN := target/$(TARGET)/release/realemail

# ring 等依赖含 C 代码，musl 构建需要一个面向 musl 的 C 编译器：
# 优先用专用 musl-gcc，没装时退回 clang（按 --target=x86_64-linux-musl 编译）。
MUSL_CC ?= $(shell command -v x86_64-linux-musl-gcc 2>/dev/null || command -v musl-gcc 2>/dev/null || command -v clang 2>/dev/null)
CC_VAR := CC_$(subst -,_,$(TARGET))
ifneq (,$(findstring musl,$(TARGET)))
CC_ENV := $(CC_VAR)="$(MUSL_CC)"
endif

.PHONY: build clean check

# 静态链接构建（release 配置）
build:
ifneq (,$(findstring musl,$(TARGET)))
	@if [ -z "$(MUSL_CC)" ]; then \
		echo "缺少 musl C 编译器，请安装 musl-gcc，或安装 clang" >&2; \
		exit 1; \
	fi
endif
	$(CC_ENV) cargo build --release --target $(TARGET)
	@echo "构建产物: $(BIN)"

# 清理全部构建产物
clean:
	cargo clean

# 快速编译检查，覆盖库、二进制与测试目标
check:
	cargo check --all-targets
