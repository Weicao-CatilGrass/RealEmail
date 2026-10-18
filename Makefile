# RealEmail 构建入口
#
# build 使用 musl 目标做静态链接，产物不依赖系统 glibc，
# 可以直接拷贝到其他 Linux 机器运行。

TARGET ?= x86_64-unknown-linux-musl
BIN := target/$(TARGET)/release/realemail

.PHONY: build clean check

# 静态链接构建（release 配置）
build:
	cargo build --release --target $(TARGET)
	@echo "构建产物: $(BIN)"

# 清理全部构建产物
clean:
	cargo clean

# 快速编译检查，覆盖库、二进制与测试目标
check:
	cargo check --all-targets
