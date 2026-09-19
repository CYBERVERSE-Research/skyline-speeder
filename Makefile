BPF_CLANG ?= clang
BPFTOOL ?= bpftool
VMLINUX_BTF ?= /sys/kernel/btf/vmlinux
PYTHON ?= .venv/bin/python
PYENV ?= pyenv
BPF_CFLAGS ?= -O2 -g -target bpf -D__TARGET_ARCH_x86
BPF_DIR := build/bpf
VMLINUX_H := bpf/include/vmlinux.h
BPF_SOURCES := bpf/skyline_cc.bpf.c bpf/skyline_policy.bpf.c bpf/skyline_tc.bpf.c
BPF_OBJECTS := $(patsubst bpf/%.bpf.c,$(BPF_DIR)/%.bpf.o,$(BPF_SOURCES))

.PHONY: all bpf rust venv check test clean

all: bpf rust

# vmlinux.h only has to DEFINE the types the BPF sources touch. It does not
# have to come from the kernel the objects will eventually run on: the header
# bpftool generates carries
# `#pragma clang attribute push (__attribute__((preserve_access_index)))`, so
# every direct field access emits a CO-RE relocation record and libbpf fixes
# the offsets at load time against whatever kernel is actually running. Verified
# by building under 6.12.63 and loading the unchanged objects on 6.19.14 --
# see infra/kernel/core-portability.sh, which makes that check repeatable.
#
# So PREBUILT_VMLINUX_H lets a build use a header generated elsewhere, which is
# what makes it possible to compile where there is no /sys/kernel/btf/vmlinux
# and no bpftool at all (a container, a release pipeline, a cross build).
ifneq ($(PREBUILT_VMLINUX_H),)
$(VMLINUX_H): $(PREBUILT_VMLINUX_H)
	@mkdir -p $(dir $@)
	cp $< $@.part
	mv $@.part $@
else
$(VMLINUX_H): $(VMLINUX_BTF)
	@mkdir -p $(dir $@)
	$(BPFTOOL) btf dump file $(VMLINUX_BTF) format c > $@.part
	mv $@.part $@
endif

$(BPF_DIR)/%.bpf.o: bpf/%.bpf.c bpf/include/skyline_abi.h $(VMLINUX_H)
	@mkdir -p $(BPF_DIR)
	$(BPF_CLANG) $(BPF_CFLAGS) -I bpf/include -c $< -o $@

bpf: $(BPF_OBJECTS)

rust:
	cargo build --workspace

.venv/bin/python:
	PYENV_VERSION=system $(PYENV) exec python3 -m venv .venv

venv: .venv/bin/python
	$(PYTHON) -m pip install --upgrade pip
	$(PYTHON) -m pip install --requirement requirements-dev.txt

check:
	bash -n infra/*.sh infra/kernel/*.sh research/experiments/*.sh
	@test -x $(PYTHON) || { echo "Run 'make venv' first" >&2; exit 1; }
	PYTHONDONTWRITEBYTECODE=1 $(PYTHON) -m unittest discover -s research/experiments/tests -v
	cargo fmt --all -- --check
	cargo check --workspace

test:
	@test -x $(PYTHON) || { echo "Run 'make venv' first" >&2; exit 1; }
	PYTHONDONTWRITEBYTECODE=1 $(PYTHON) -m unittest discover -s research/experiments/tests -v
	cargo test --workspace

clean:
	@echo "Refusing to remove build outputs automatically; remove build/ manually after reviewing its contents."
