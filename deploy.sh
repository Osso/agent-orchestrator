#!/usr/bin/env bash
set -euo pipefail

echo "Building release..."
cargo build --release

echo "Stopping service..."
systemctl --user stop agent-orchestrator || true
sleep 1

echo "Installing binary..."
rm -f ~/.local/bin/agent-orchestrator
cp target/release/agent-orchestrator ~/.local/bin/agent-orchestrator

echo "Starting service..."
systemctl --user start agent-orchestrator

sleep 1
systemctl --user status agent-orchestrator --no-pager
