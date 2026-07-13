SHELL := /usr/bin/env bash

ROOT := $(abspath $(dir $(lastword $(MAKEFILE_LIST))))
OUT ?= $(ROOT)/out
JOBS ?= $(shell getconf _NPROCESSORS_ONLN 2>/dev/null || echo 1)

.PHONY: all fetch configure build verify clean distclean print-artifacts rebuild-agent rebuild-display rebuild-net

all: build

fetch configure build verify print-artifacts:
	@OUT="$(OUT)" JOBS="$(JOBS)" "$(ROOT)/scripts/build.sh" $@

# Rebuild exactly one local DVM relay without discarding the Buildroot host
# toolchain. The normal `build` target detects source changes automatically.
rebuild-agent:
	@OUT="$(OUT)" JOBS="$(JOBS)" "$(ROOT)/scripts/build.sh" rebuild-agent

rebuild-display:
	@OUT="$(OUT)" JOBS="$(JOBS)" "$(ROOT)/scripts/build.sh" rebuild-display

rebuild-net:
	@OUT="$(OUT)" JOBS="$(JOBS)" "$(ROOT)/scripts/build.sh" rebuild-net

clean:
	@OUT="$(OUT)" "$(ROOT)/scripts/build.sh" clean

distclean:
	@OUT="$(OUT)" "$(ROOT)/scripts/build.sh" distclean
