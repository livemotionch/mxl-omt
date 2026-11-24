#!/usr/bin/env bash
# Copy this checkout to the mxl-test VM, build lm-mxl-omt:dev there, restart the
# test instance (~/lm-mxl-omt-run, compose project lm-omt).
set -euo pipefail
host="${1:-ubuntu@200.234.112.226}"
cd "$(dirname "$0")/.."
rsync -a --delete --exclude target --exclude .git ./ "$host:lm-mxl-omt/"
ssh "$host" 'cd ~/lm-mxl-omt && docker build -q --build-arg UBUNTU=public.ecr.aws/docker/library/ubuntu:24.04 -f docker/Dockerfile -t lm-mxl-omt:dev . \
  && cd ~/lm-mxl-omt-run && docker compose -p lm-omt up -d --force-recreate 2>&1 | tail -1'
