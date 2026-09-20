# Day-to-day commands for developing and operating claude-job-runner.
#
# "Local" targets run on your machine. "Server" targets are meant to be run
# on the EC2 host (as the `ubuntu` user, they use sudo); `make deploy` and
# the `remote-*` targets run them over SSH for you.

EDITOR   ?= vim
SERVICE  := claude-job-runner
APP_DIR  := /home/runner/claude-job-runner
SECRETS  := /etc/claude-job-runner/secrets.env
AS_RUNNER := sudo -u runner -i

# SSH settings for the remote-* targets (override: make deploy HOST=1.2.3.4).
HOST     ?= 3.135.213.87
KEY      ?= ~/.ssh/claude-job-runner.pem
SSH      := ssh -i $(KEY) ubuntu@$(HOST)

.DEFAULT_GOAL := help

.PHONY: help build test lint run \
        env secrets update restart stop start status logs health job \
        ssh deploy remote-logs remote-status remote-env

help: ## List available targets
	@grep -E '^[a-zA-Z_-]+:.*?## ' $(MAKEFILE_LIST) | sort | \
	  awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-14s\033[0m %s\n", $$1, $$2}'

# --- Local development -------------------------------------------------------

build: ## Build the release binary
	cargo build --release

test: ## Run unit + integration tests (needs DATABASE_URL in .env)
	cargo test

lint: ## Run clippy on all targets
	cargo clippy --all-targets

run: ## Run the daemon locally in the foreground
	cargo run --release

# --- Server operations (run on the EC2 host) --------------------------------

env: ## Edit the runner's .env, then restart the service
	$(AS_RUNNER) $(EDITOR) $(APP_DIR)/.env
	$(MAKE) restart

secrets: ## Edit the Claude token file (root only), then restart the service
	sudo $(EDITOR) $(SECRETS)
	$(MAKE) restart

update: ## Pull main, rebuild, restart
	$(AS_RUNNER) sh -c 'cd $(APP_DIR) && git pull --ff-only && ~/.cargo/bin/cargo build --release'
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

job: ## Submit a job: make job CHAT=12 PROMPT="Say hello"
	@test -n "$(CHAT)" || { echo "usage: make job CHAT=<chat_id> PROMPT=\"...\""; exit 1; }
	@CHAT="$(CHAT)" PROMPT="$(PROMPT)" python3 -c 'import json,os; print(json.dumps({"chat_id": int(os.environ["CHAT"]), "content": os.environ["PROMPT"]}))' \
	  | curl -s -X POST localhost:8080/jobs -H 'content-type: application/json' -d @-; echo

# --- Remote shortcuts (run from your laptop) --------------------------------

ssh: ## Open a shell on the server
	$(SSH)

deploy: ## Push main to GitHub, then update + restart on the server
	git push origin main
	$(SSH) 'make -C $(APP_DIR) update'

remote-logs: ## Follow the service log from here
	$(SSH) -t 'make -C $(APP_DIR) logs'

remote-status: ## Show service status from here
	$(SSH) 'make -C $(APP_DIR) status'

remote-env: ## Edit the server .env from here
	$(SSH) -t 'make -C $(APP_DIR) env'
