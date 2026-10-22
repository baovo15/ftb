#!/usr/bin/env bash
# Build ftb (Fabro fork) from source, push it to the nhaboc Nexus registry and (re)deploy it to k3s.
# Run on the k3s host (beboc) as a user with sudo, docker and kubectl:
#   curl -fsSL https://raw.githubusercontent.com/baovo15/ftb/main/scripts/ftb-deploy.sh | bash
# Re-running it is a redeploy: it pulls main, builds a new tag and rolls the
# ftb StatefulSet. The town data under /var/lib/ftb survives.
set -euo pipefail

REPO_URL=${REPO_URL:-https://github.com/baovo15/ftb.git}
SRC=${SRC:-$HOME/src/ftb}
REGISTRY=${REGISTRY:-127.0.0.1:30082}
NEXUS_USER=${NEXUS_USER:-ci}
NEXUS_PASSWORD_FILE=${NEXUS_PASSWORD_FILE:-/etc/tieudidi/nexus-ci.password}
NS=ftb

step() { printf '\n==> %s\n' "$*"; }

step "Source: $REPO_URL -> $SRC"
if [ -d "$SRC/.git" ]; then
  git -C "$SRC" fetch -q origin && git -C "$SRC" checkout -q main && git -C "$SRC" reset -q --hard origin/main
else
  git clone -q "$REPO_URL" "$SRC"
fi
TAG=$(git -C "$SRC" rev-parse --short HEAD)
IMAGE="$REGISTRY/ftb:$TAG"

step "Host volume /var/lib/ftb (the entrypoint chowns it to fabro)"
sudo mkdir -p /var/lib/ftb
[ -S /var/run/docker.sock ] || { echo "no /var/run/docker.sock on this host; ftb needs it for Docker sandboxes" >&2; exit 1; }

step "Build $IMAGE"
DOCKER_BUILDKIT=1 docker build --platform linux/amd64 -f "$SRC/deploy/Dockerfile.source" \
  --build-arg GIT_SHA="$TAG" -t "$IMAGE" -t "$REGISTRY/ftb:latest" "$SRC"

step "Push to Nexus ($REGISTRY, user $NEXUS_USER)"
sudo cat "$NEXUS_PASSWORD_FILE" | docker login "$REGISTRY" -u "$NEXUS_USER" --password-stdin >/dev/null
docker push -q "$IMAGE"
docker push -q "$REGISTRY/ftb:latest"

step "Namespace and session secret (generated once, kept on redeploy)"
kubectl get ns "$NS" >/dev/null 2>&1 || kubectl create ns "$NS"
kubectl -n "$NS" get secret ftb-env >/dev/null 2>&1 \
  || kubectl -n "$NS" create secret generic ftb-env --from-literal=SESSION_SECRET="$(openssl rand -hex 32)"

step "Apply manifests"
sed "s|IMAGE_TAG|$TAG|" "$SRC/deploy/k3s/ftb.yaml" | kubectl apply -f -
kubectl -n "$NS" rollout status sts/ftb --timeout=10m

step "Edge: ftb.nhaboc.com -> 127.0.0.1:30882"
# Clone the gastown vhost the first time; nginx config is host-owned, so
# anything unexpected is reported rather than guessed.
src_conf=$(sudo grep -rl 'gastown.nhaboc.com' /etc/nginx 2>/dev/null | head -1 || true)
if sudo grep -rqs 'ftb.nhaboc.com' /etc/nginx; then
  echo "nginx already serves ftb.nhaboc.com"
elif [ -n "$src_conf" ] && [ "$(sudo grep -c server_name "$src_conf")" -eq 1 ]; then
  dst_conf=$(dirname "$src_conf")/ftb.conf
  sudo sed -e 's/gastown\.nhaboc\.com/ftb.nhaboc.com/g' -e 's/30880/30882/g' "$src_conf" | sudo tee "$dst_conf" >/dev/null
  [ -d /etc/nginx/sites-enabled ] && [ "$(dirname "$dst_conf")" = /etc/nginx/sites-available ] \
    && sudo ln -sf "$dst_conf" /etc/nginx/sites-enabled/ftb.conf
  sudo nginx -t && sudo systemctl reload nginx
  echo "wrote $dst_conf from $src_conf"
else
  echo "MANUAL: no single-vhost nginx file for gastown.nhaboc.com found (got: ${src_conf:-none})."
  echo "        Add ftb.nhaboc.com -> 127.0.0.1:30882 next to the gastown entry, and 30882 to TERMINATOR_PORTS if nginx reaches NodePorts via the firewall."
fi

step "First run: install wizard token"
kubectl -n "$NS" logs ftb-0 --tail=200 | grep -i -E 'install (url|token)' || echo "no install token in logs (already installed)"

echo
echo "Deployed $IMAGE -> https://ftb.nhaboc.com (DNS: add a Cloudflare DNS-only A record ftb -> 100.76.86.103 if missing)"
