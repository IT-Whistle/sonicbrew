#!/bin/sh
# sonicbrew FreeBSD regression suite — one-shot native verification.
# Runs on the dedicated FreeBSD test machine (15.1-RELEASE-p2).
# Usage: sh regression.sh [/path/to/sonicbrew]
#
# Layers (TESTING-STANDARDS Layer 4):
#   1. fmt --check + clippy -D warnings      (host parity)
#   2. cargo build --workspace               (native build)
#   3. cargo test --workspace                (full suite)
#   4. binary self-tests (5)                 (deterministic)
#   5. server-engine smoke: REST live reload + preset persistence
#   6. gw-pulse handshake against a local PulseAudio daemon
#   7. netmap capability probe (vale1:1 + first NIC)      [features netmap]
#
# Exit code: number of FAILED sections (0 = all green).

SRC="${1:-/root/sonicbrew}"
cd "$SRC" || { echo "FATAL: no source at $SRC"; exit 99; }
FAIL=0
step() { printf '\n=== [%s] %s ===\n' "$(date +%H:%M:%S)" "$1"; }
fail() { echo "!! FAILED: $1"; FAIL=$((FAIL+1)); }

step "1/8 fmt + clippy"
cargo fmt --all -- --check || fail fmt
cargo clippy --workspace --all-targets -- -D warnings > /dev/null 2>&1 || fail clippy

step "2/8 workspace build"
cargo build --workspace > /tmp/reg-build.log 2>&1 || { fail build; tail -5 /tmp/reg-build.log; }

step "3/8 workspace tests"
cargo test --workspace --no-fail-fast > /tmp/reg-test.log 2>&1
PASSED=$(grep -h '^test result: ok' /tmp/reg-test.log | awk -F'[ ;]' '{s+=$4} END{print s+0}')
FAILED_T=$(grep -h '^test result:' /tmp/reg-test.log | awk -F'[ ;]' '{s+=$8} END{print s+0}')
echo "passed=$PASSED failed=$FAILED_T"
[ "$FAILED_T" = "0" ] && [ "$PASSED" -gt 400 ] || fail "tests (passed=$PASSED failed=$FAILED_T)"

step "4/8 self-tests (5)"
for t in self-test hot-reload-test live-rebuild-test engine-live-rebuild-test gateway-live-reload-test; do
  timeout 60 ./target/debug/sonicbrew --$t > /dev/null 2>&1 && echo "  ok: --$t" || fail "self-test --$t"
done

step "5/8 server-engine smoke (REST + persistence)"
rm -f /tmp/sonicbrew-dev.redb /tmp/sonicbrew-dev.preset.json
./target/debug/sonicbrew --server-engine --api-addr 127.0.0.1:9042 \
  --ws-addr 127.0.0.1:9041 --metrics-addr 127.0.0.1:9043 > /tmp/reg-server.log 2>&1 &
SRV=$!
sleep 6
curl -s -X POST http://127.0.0.1:9042/nodes -H 'content-type: application/json' \
  -d '{"label":"eq1","inputs":1,"outputs":1,"kind":"eq","params":{"Eq":{"freq":2000,"q":0.9}}}' | grep -q '"id"' || fail "REST POST /nodes"
curl -s http://127.0.0.1:9042/topology | grep -q '"kind":"eq"' || fail "GET /topology"
# Let the 2 s autosave capture the eq node BEFORE the kill.
sleep 3
kill $SRV 2>/dev/null; wait $SRV 2>/dev/null
# Wait out the redb lock release: poll until the process is truly gone and
# the lock is free (up to 10 s — slow after a fresh reboot).
i=0
while pgrep -f "sonicbrew --server-engine" > /dev/null && [ $i -lt 20 ]; do
  sleep 0.5; i=$((i+1))
done
sleep 1
test -s /tmp/sonicbrew-dev.preset.json || fail "preset autosave"
./target/debug/sonicbrew --server-engine --api-addr 127.0.0.1:9042 \
  --ws-addr 127.0.0.1:9041 --metrics-addr 127.0.0.1:9043 > /tmp/reg-server2.log 2>&1 &
SRV=$!
sleep 6
curl -s http://127.0.0.1:9042/nodes | grep -q '"kind":"eq"' || fail "preset restore after restart"
kill $SRV 2>/dev/null; wait $SRV 2>/dev/null
rm -f /tmp/sonicbrew-dev.redb /tmp/sonicbrew-dev.preset.json

step "6/8 gw-pulse handshake (live PulseAudio)"
pgrep pulseaudio > /dev/null || { pulseaudio -D --exit-idle-time=-1 > /dev/null 2>&1; sleep 3; }
cargo build -p gw-pulse --example handshake > /dev/null 2>&1
timeout 20 ./target/debug/examples/handshake > /tmp/reg-pulse.log 2>&1 \
  && grep -q "handshake OK" /tmp/reg-pulse.log || fail "gw-pulse handshake"

step "7/8 netmap ring I/O (vale1:1)"
cargo build -p net-rtp-aes67 --features netmap --example netmap_probe > /dev/null 2>&1
if [ -e /dev/netmap ]; then
  timeout 20 ./target/debug/examples/netmap_probe vale1:1 > /tmp/reg-nm.log 2>&1 \
    && grep -q "netmap api   : requested 14, kernel 14" /tmp/reg-nm.log || fail "netmap probe vale1:1"
  # Ring I/O: register + send + txsync slot accounting.
  cargo build -p net-rtp-aes67 --features netmap --example nm_port_test > /dev/null 2>&1
  timeout 20 ./target/debug/examples/nm_port_test > /tmp/reg-nmport.log 2>&1 \
    && grep -q "PORT_TEST_PASS" /tmp/reg-nmport.log || fail "netmap ring I/O vale1:1"
else
  echo "  skipped: /dev/netmap absent"
fi

step "8/8 netmap RTP loopback (vale60 switch)"
if [ -e /dev/netmap ]; then
  cargo build -p net-rtp-aes67 --features netmap --example vale_loopback > /dev/null 2>&1
  timeout 40 ./target/debug/examples/vale_loopback > /tmp/reg-valelb.log 2>&1 \
    && grep -q "LOOPBACK_PASS" /tmp/reg-valelb.log || fail "netmap RTP loopback"
else
  echo "  skipped: /dev/netmap absent"
fi

printf '\n========================================\n'
if [ "$FAIL" = "0" ]; then
  echo "REGRESSION: ALL GREEN (tests=$PASSED)"
else
  echo "REGRESSION: $FAIL SECTION(S) FAILED"
fi
exit $FAIL
