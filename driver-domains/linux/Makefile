SHELL := /usr/bin/env bash

ROOT := $(abspath $(dir $(lastword $(MAKEFILE_LIST))))
OUT ?= $(ROOT)/out
JOBS ?= $(shell getconf _NPROCESSORS_ONLN 2>/dev/null || echo 1)

.PHONY: all fetch configure build verify clean distclean print-artifacts

all: build

fetch configure build verify print-artifacts:
	@OUT="$(OUT)" JOBS="$(JOBS)" "$(ROOT)/scripts/build.sh" $@

clean:
	@OUT="$(OUT)" "$(ROOT)/scripts/build.sh" clean

distclean:
	@OUT="$(OUT)" "$(ROOT)/scripts/build.sh" distclean
