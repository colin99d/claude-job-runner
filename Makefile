# Day-to-day commands for developing and operating claude-job-runner.
#
# Local targets (build, test, ...) run where you are. Server targets (env,
# update, logs, ...) run on the EC2 host: invoked there they use sudo, invoked
# from your laptop they forward themselves over SSH, so `make logs` works from
# either side.

EDITOR    ?= vim
SERVICE   := claude-job-runner
APP_DIR   := /home/runner/claude-job-runner
SECRETS   := /etc/claude-job-runner/secrets.env
AS_RUNNER := sudo -u runner -i

# SSH settings for forwarding (override: make logs HOST=1.2.3.4).
HOST ?= 3.16.79.74
KEY  ?= ~/.ssh/claude-job-runner.pem
SSH  := ssh -i $(KEY) ubuntu@$(HOST)

SERVER_TARGETS := env secrets update restart stop start status logs health job
ON_SERVER := $(shell test -d $(APP_DIR) && echo 1)

.DEFAULT_GOAL := help
.PHONY: help build test lint run ssh deploy $(SERVER_TARGETS)

help: ## List available targets
	@grep -E '^[a-zA-Z_-]+:.*?## ' $(MAKEFILE_LIST) | sort | \
	  awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-10s\033[0m %s\n", $$1, $$2}'

# --- Local development -------------------------------------------------------

build: ## Build the daemon, jobctl and jobctl-mcp
	cargo build --release --workspace

test: ## Run unit + integration tests (needs DATABASE_URL in .env)
	cargo test --workspace

lint: ## Run clippy on all targets
	cargo clippy --workspace --all-targets

run: ## Run the daemon locally in the foreground
	cargo run --release --bin claude-job-runner

ssh: ## Open a shell on the server
	$(SSH)

deploy: ## Push main to GitHub, then update + restart on the server
	git push origin main
	$(MAKE) update

# --- Server operations -------------------------------------------------------

ifeq ($(ON_SERVER),1)

env: ## Edit the runner's .env, then restart the service
	$(AS_RUNNER) $(EDITOR) $(APP_DIR)/.env
	$(MAKE) restart

secrets: ## Edit the Claude token file (root only), then restart the service
	sudo $(EDITOR) $(SECRETS)
	$(MAKE) restart

update: ## Pull main, rebuild, restart
	$(AS_RUNNER) sh -c 'cd $(APP_DIR) && git pull --ff-only && ~/.cargo/bin/cargo build --release --workspace'
	$(MAKE) restart

restart: ## Restart the service and show its status
	sudo systemctl restart $(SERVICE)
	@sleep 2
	$(MAKE) status

stop: ## Stop the service (running jobs are killed and marked failed)
	sudo systemctl stop $(SERVICE)

start: ## Start the service
	sudo systemctl start $(SERVICE)

status: ## Show service status and the last log lines
	sudo systemctl status $(SERVICE) --no-pager -l | head -20

logs: ## Follow the service log
	sudo journalctl -u $(SERVICE) -f -o cat

health: ## Hit the local HTTP API
	curl -s localhost:8080/health; echo

job: ## Run a job and print the answer: make job CHAT=12 PROMPT="Say hello"
	@test -n "$(CHAT)" || { echo "usage: make job CHAT=<chat_id> PROMPT=\"...\""; exit 1; }
	@$(APP_DIR)/target/release/jobctl ask --no-start --chat "$(CHAT)" -- "$(PROMPT)"

else

# Not on the server: run the same target there. -t gives vim/journalctl a tty.
# sq wraps a value in single quotes for the remote shell, escaping any inside.
sq = '$(subst ','\'',$(1))'
$(SERVER_TARGETS):
	$(SSH) -t "make --no-print-directory -C $(APP_DIR) $@ EDITOR=$(call sq,$(EDITOR)) CHAT=$(call sq,$(CHAT)) PROMPT=$(call sq,$(PROMPT))"

endif
