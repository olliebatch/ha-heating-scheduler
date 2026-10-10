#!/usr/bin/env bash
# Try the scheduler and its panel on fake data: mock climates, mock areas and mock weather.
# Nothing talks to a real Home Assistant, and your clones' working trees and branches are left alone.
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: scripts/demo.sh [backend-ref] [panel-ref]

Runs the debug backend (mock climates, areas and weather) on :3000 and the panel dev server, from
throwaway git worktrees of each ref. Ctrl-C stops both and removes the worktrees.

  backend-ref  Default: this clone's current branch (or commit, if detached).
  panel-ref    Default: the panel branch with the same name as backend-ref (local, else origin/),
               else main. So running it from a PR branch demos that PR and its panel PR.

The panel clone is ha-heating-scheduler-panel next to this repo (its main clone), or PANEL_DIR.
Needs: Rust (cargo), Node 24 with corepack, curl.
Environment:
  PANEL_DIR         Panel clone (default: ../ha-heating-scheduler-panel)
  PANEL_PORT        Panel dev server port (default: 5173)
  DEMO_TARGET_DIR   Cargo build cache, kept between runs
                    (default: ${XDG_CACHE_HOME:-~/.cache}/ha-heating-scheduler-demo/target)
EOF
}

case "${1:-}" in -h|--help) usage; exit 0;; esac
[ $# -le 2 ] || { usage >&2; exit 2; }

backend_dir=$(cd "$(dirname "$0")/.." && pwd)
# The main clone, even when this runs from one of its worktrees
main_clone=$(dirname "$(cd "$backend_dir" && cd "$(git rev-parse --git-common-dir)" && pwd)")
panel_dir=${PANEL_DIR:-$(dirname "$main_clone")/ha-heating-scheduler-panel}
panel_port=${PANEL_PORT:-5173}
target_dir=${DEMO_TARGET_DIR:-${XDG_CACHE_HOME:-$HOME/.cache}/ha-heating-scheduler-demo/target}

die() { echo "demo: $*" >&2; exit 1; }
for tool in cargo node corepack curl git; do
  command -v "$tool" >/dev/null || die "$tool not found (needs Rust, Node 24 with corepack, curl)"
done
node_major=$(node -p 'process.versions.node.split(".")[0]')
[ "$node_major" -ge 24 ] || echo "demo: warning: Node $(node -v) found; the panel expects Node 24" >&2
git -C "$panel_dir" rev-parse --git-dir >/dev/null 2>&1 || die "no panel clone at $panel_dir (set PANEL_DIR)"
# Busy unless nothing accepts the connection (curl 7) or it times out (28). Ask localhost, which
# curl tries over IPv6 and IPv4: vite listens on whichever localhost resolves to first.
port_busy() {
  local rc=0
  curl -s -o /dev/null --max-time 1 "http://localhost:$1/" || rc=$?
  [ "$rc" -ne 7 ] && [ "$rc" -ne 28 ]
}
port_busy 3000 && die "something is already listening on :3000; stop it first"
port_busy "$panel_port" && die "something is already listening on :$panel_port; set PANEL_PORT"

backend_ref=${1:-$(git -C "$backend_dir" symbolic-ref --quiet --short HEAD || git -C "$backend_dir" rev-parse HEAD)}
git -C "$backend_dir" rev-parse --verify --quiet "$backend_ref^{commit}" >/dev/null || die "unknown backend ref: $backend_ref"
if [ $# -ge 2 ]; then
  panel_ref=$2
elif git -C "$panel_dir" rev-parse --verify --quiet "refs/heads/$backend_ref" >/dev/null; then
  panel_ref=$backend_ref
elif git -C "$panel_dir" rev-parse --verify --quiet "refs/remotes/origin/$backend_ref" >/dev/null; then
  panel_ref=origin/$backend_ref
else
  panel_ref=main
fi
git -C "$panel_dir" rev-parse --verify --quiet "$panel_ref^{commit}" >/dev/null || die "unknown panel ref: $panel_ref"

tmp=$(mktemp -d "${TMPDIR:-/tmp}/heating-demo.XXXXXX")
pids=()
cleanup() {
  trap - EXIT INT TERM
  for pid in "${pids[@]}"; do kill "$pid" 2>/dev/null || true; done
  for pid in "${pids[@]}"; do wait "$pid" 2>/dev/null || true; done
  git -C "$backend_dir" worktree remove --force "$tmp/backend" 2>/dev/null || true
  git -C "$panel_dir" worktree remove --force "$tmp/panel" 2>/dev/null || true
  rm -rf "$tmp"
  echo "demo: stopped and cleaned up"
}
trap cleanup EXIT
trap 'exit 130' INT TERM

echo "demo: backend $backend_ref, panel $panel_ref"
git -C "$backend_dir" worktree add --quiet --detach "$tmp/backend" "$backend_ref"
git -C "$panel_dir" worktree add --quiet --detach "$tmp/panel" "$panel_ref"

echo "demo: building the debug backend (cache: $target_dir)"
cargo build --quiet --manifest-path "$tmp/backend/Cargo.toml" --target-dir "$target_dir"
echo "demo: installing panel dependencies"
(cd "$tmp/panel" && corepack pnpm install --frozen-lockfile --silent)

# Run from an empty folder with fake settings, so dotenv can't find a real .env and nothing reaches a real HA.
# Port 9 on localhost has nothing listening; debug builds use mocks anyway.
mkdir -p "$tmp/run" "$tmp/data"
(
  cd "$tmp/run"
  exec env HA_URL=http://127.0.0.1:9 HA_TOKEN=fake DATA_PATH="$tmp/data" \
    CLIMATE_ENTITY=climate.lounge_trv,climate.study_trv,climate.study_trv_2,climate.hall_trv \
    "$target_dir/debug/ha-heating-scheduler"
) >"$tmp/backend.log" 2>&1 &
pids+=($!)
# Run vite directly (not through pnpm) so stopping it stops the server
(cd "$tmp/panel" && exec env -u VITE_API_BASE_URL ./node_modules/.bin/vite --port "$panel_port" --strictPort) \
  >"$tmp/panel.log" 2>&1 &
pids+=($!)

show_logs() { for log in "$tmp"/*.log; do echo "--- $(basename "$log")" >&2; tail -n 20 "$log" >&2; done; }
wait_for() {
  for _ in $(seq 1 60); do
    port_busy "$1" && return 0
    for pid in "${pids[@]}"; do kill -0 "$pid" 2>/dev/null || { show_logs; die "$2 exited"; }; done
    sleep 0.5
  done
  show_logs; die "$2 didn't start"
}
wait_for 3000 "backend"
wait_for "$panel_port" "panel"

cat <<EOF

  Panel:   http://localhost:$panel_port
  Backend: http://localhost:3000 (logs: $tmp/backend.log)

  What to try (the scheduler ticks every 15 s):
  - Zones: Lounge and Study come from the mock areas; climate.hall_trv falls into "Whole house".
  - Make a second schedule set, then pick it for one zone.
  - Open a zone's weather profile, turn "Adjust for the weather" on, set its wind exposure and a sun window.
  - Change the mock weather, then watch the zone's why line on the next tick:
      curl -X POST localhost:3000/weather/mock -H 'content-type: application/json' \\
        -d '{"temperature": -3, "wind_speed": 40, "cloud_coverage": 10}'
  - Early start: add an On period starting about 20 minutes from now; with the cold weather above, the
    zone turns on early and its why line says so.

  Ctrl-C to stop.
EOF
wait "${pids[@]}"
