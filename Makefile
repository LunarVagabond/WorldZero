-include .env
export

CARGO ?= cargo
RUN_DIR := _running
PID_DIR := $(RUN_DIR)/pids
LOG_DIR := $(RUN_DIR)/logs
PID_FILE := $(PID_DIR)/server.pid
LOG_FILE := $(LOG_DIR)/server.log

.PHONY: help build run start stop restart status test test-live fmt fmt-check lint check clean

help:
	@echo "WorldZero — local dev commands"
	@echo ""
	@echo "  make build       cargo build --workspace"
	@echo "  make run         run the server binary in the foreground"
	@echo "  make start       run the server binary in the background (pid/log under $(RUN_DIR)/)"
	@echo "  make stop        stop the background server started with 'make start'"
	@echo "  make restart     stop, then start"
	@echo "  make status      report whether the background server is running"
	@echo "  make test        cargo test --workspace"
	@echo "  make test-live   also run tests gated on real infra (needs WZ_POSTGRES_*/WZ_REDIS_* — .env is loaded automatically)"
	@echo "  make fmt         cargo fmt"
	@echo "  make fmt-check   cargo fmt --all -- --check"
	@echo "  make lint        cargo clippy --workspace --all-targets -- -D warnings"
	@echo "  make check       fmt-check + lint + test (what CI runs)"
	@echo "  make clean       cargo clean, remove PID/log files"

build:
	$(CARGO) build --workspace

run:
	$(CARGO) run -p server

start:
	@mkdir -p $(PID_DIR) $(LOG_DIR)
	@if [ -f $(PID_FILE) ] && kill -0 "$$(cat $(PID_FILE))" 2>/dev/null; then \
		echo "server already running (pid $$(cat $(PID_FILE)))"; \
	else \
		$(CARGO) build -p server; \
		( $(CARGO) run -p server > $(LOG_FILE) 2>&1 & echo $$! > $(PID_FILE) ); \
		sleep 1; \
		echo "server started (pid $$(cat $(PID_FILE))), logs: $(LOG_FILE)"; \
	fi

stop:
	@if [ -f $(PID_FILE) ] && kill -0 "$$(cat $(PID_FILE))" 2>/dev/null; then \
		kill "$$(cat $(PID_FILE))"; \
		rm -f $(PID_FILE); \
		echo "server stopped"; \
	else \
		echo "server not running"; \
		rm -f $(PID_FILE); \
	fi

restart: stop start

status:
	@if [ -f $(PID_FILE) ] && kill -0 "$$(cat $(PID_FILE))" 2>/dev/null; then \
		echo "server running (pid $$(cat $(PID_FILE)))"; \
	else \
		echo "server not running"; \
	fi

test:
	$(CARGO) test --workspace

test-live:
	$(CARGO) test --workspace -- --ignored

fmt:
	$(CARGO) fmt

fmt-check:
	$(CARGO) fmt --all -- --check

lint:
	$(CARGO) clippy --workspace --all-targets -- -D warnings

check: fmt-check lint test

clean:
	$(CARGO) clean
	rm -rf $(RUN_DIR)
