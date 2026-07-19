SHELL := /usr/bin/env bash

ROOT := $(abspath $(dir $(lastword $(MAKEFILE_LIST))))
OUT ?= $(ROOT)/out
JOBS ?= $(shell getconf _NPROCESSORS_ONLN 2>/dev/null || echo 1)

.PHONY: all fetch configure build verify stage-release clean distclean print-artifacts rebuild-agent rebuild-display rebuild-net dev-agent dev-display dev-net selftest-config-cache build-plan ccache-stats profile-build

all: build

fetch configure build verify print-artifacts selftest-config-cache build-plan ccache-stats profile-build:
	@OUT="$(OUT)" JOBS="$(JOBS)" "$(ROOT)/scripts/build.sh" $@

# Rebuild exactly one local DVM relay without discarding the Buildroot host
# toolchain. The normal `build` target detects source changes automatically.
rebuild-agent:
	@OUT="$(OUT)" JOBS="$(JOBS)" "$(ROOT)/scripts/build.sh" rebuild-agent

rebuild-display:
	@OUT="$(OUT)" JOBS="$(JOBS)" "$(ROOT)/scripts/build.sh" rebuild-display

rebuild-net:
	@OUT="$(OUT)" JOBS="$(JOBS)" "$(ROOT)/scripts/build.sh" rebuild-net

# Compile one relay against the cached DVM sysroot without producing a rootfs,
# manifest, or release artifact.  rebuild-* is the one-time integration step.
dev-agent:
	@OUT="$(OUT)" JOBS="$(JOBS)" "$(ROOT)/scripts/build.sh" dev-agent

dev-display:
	@OUT="$(OUT)" JOBS="$(JOBS)" "$(ROOT)/scripts/build.sh" dev-display

dev-net:
	@OUT="$(OUT)" JOBS="$(JOBS)" "$(ROOT)/scripts/build.sh" dev-net

stage-release:
	@test -n "$(DEST)" || { echo 'usage: make stage-release DEST=/trusted/absolute/release/path' >&2; exit 1; }
	@OUT="$(OUT)" JOBS="$(JOBS)" "$(ROOT)/scripts/build.sh" verify
	@"$(ROOT)/scripts/stage-release.sh" "$(OUT)/artifacts" "$(DEST)"

clean:
	@OUT="$(OUT)" "$(ROOT)/scripts/build.sh" clean

distclean:
	@OUT="$(OUT)" "$(ROOT)/scripts/build.sh" distclean
