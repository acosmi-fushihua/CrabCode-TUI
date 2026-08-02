.PHONY: all build check test test-rust test-memory test-search test-account-bridge smoke

all: check test

build:
	bun run build

check:
	bun run check

test:
	bun run test

test-rust:
	bun run test:rust

test-memory:
	bun run test:memory

test-search:
	bun run test:search

test-account-bridge:
	bun run test:account-bridge

smoke:
	bun run smoke:tui
