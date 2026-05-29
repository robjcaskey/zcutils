RUSTC ?= rustc
CARGO_NIGHTLY ?= /home/rob/.rustup/toolchains/nightly-x86_64-unknown-linux-gnu/bin/cargo
RUSTC_NIGHTLY ?= /home/rob/.rustup/toolchains/nightly-x86_64-unknown-linux-gnu/bin/rustc
LLC ?= $(shell if [ -x /home/rob/.cache/bazel/_bazel_rob/6c2345e69adc18816c6eaf60887ebd69/external/rules_rust++rust+rust_linux_x86_64__x86_64-unknown-linux-gnu__stable_tools/lib/rustlib/x86_64-unknown-linux-gnu/bin/llc ]; then printf '%s\n' /home/rob/.cache/bazel/_bazel_rob/6c2345e69adc18816c6eaf60887ebd69/external/rules_rust++rust+rust_linux_x86_64__x86_64-unknown-linux-gnu__stable_tools/lib/rustlib/x86_64-unknown-linux-gnu/bin/llc; elif command -v llc-21 >/dev/null 2>&1; then command -v llc-21; elif command -v llc-20 >/dev/null 2>&1; then command -v llc-20; else command -v llc-18 || command -v llc; fi)
TARGET_DIR ?= target

BPF_TARGET ?= bpfel-unknown-none
BPF_CARGO_TARGET_DIR ?= $(TARGET_DIR)/cargo
BPF_BC := $(TARGET_DIR)/zc_raft_fastpath.bc

.PHONY: all clean

all: $(TARGET_DIR)/zc_raft_fastpath.o $(TARGET_DIR)/zc_raft_bpf_ctl

$(TARGET_DIR):
	mkdir -p $@

$(BPF_BC): zc_raft_fastpath_kern.rs Cargo.toml | $(TARGET_DIR)
	RUSTC=$(RUSTC_NIGHTLY) \
	RUSTC_BOOTSTRAP=1 $(CARGO_NIGHTLY) -Z build-std=core,compiler_builtins \
		-Z build-std-features=compiler-builtins-mem \
		rustc --manifest-path Cargo.toml --target $(BPF_TARGET) --release \
		--target-dir $(BPF_CARGO_TARGET_DIR) --lib -- \
		-C panic=abort -C opt-level=2 -C debuginfo=2 --emit=llvm-bc=$(abspath $@)

$(TARGET_DIR)/zc_raft_fastpath.o: $(BPF_BC)
	$(LLC) -march=bpfel -filetype=obj $< -o $@

$(TARGET_DIR)/zc_raft_bpf_ctl: zc_raft_bpf_ctl.rs | $(TARGET_DIR)
	$(RUSTC) --edition=2021 -C opt-level=2 -C debuginfo=1 $< -o $@

clean:
	rm -rf $(TARGET_DIR)
