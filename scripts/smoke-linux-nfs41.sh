#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RUN_ID="$(date +%Y%m%d-%H%M%S)"
ARTIFACT_DIR="${ARTIFACT_DIR:-/tmp/embednfs-linux-smoke-${RUN_ID}}"
BACKING_DIR="${BACKING_DIR:-${ARTIFACT_DIR}/backing}"
MOUNT_DIR="${MOUNT_DIR:-${ARTIFACT_DIR}/mnt}"
SERVER_LOG="${SERVER_LOG:-${ARTIFACT_DIR}/server.log}"
SERVER_PID_FILE="${SERVER_PID_FILE:-${ARTIFACT_DIR}/server.pid}"
SUMMARY_FILE="${SUMMARY_FILE:-${ARTIFACT_DIR}/summary.tsv}"
SERVER_PORT="${SERVER_PORT:-12049}"
SERVER_ADDR="${SERVER_ADDR:-127.0.0.1:${SERVER_PORT}}"
CONTROL_PORT="${CONTROL_PORT:-12050}"
CONTROL_ADDR="${CONTROL_ADDR:-127.0.0.1:${CONTROL_PORT}}"
SERVER_CMD="${SERVER_CMD:-cargo run -p embednfsd --release}"
SERVER_RUST_LOG="${SERVER_RUST_LOG:-embednfs=debug,embednfsd=info}"
DIRECTORY_DELEGATIONS="${DIRECTORY_DELEGATIONS:-0}"
REQUIRE_DELEGATIONS="${REQUIRE_DELEGATIONS:-0}"
PRODUCT_BEHAVIOR="${PRODUCT_BEHAVIOR:-0}"
RECALL_TIMEOUT_MS="${RECALL_TIMEOUT_MS:-1000}"
VISIBILITY_TIMEOUT_MS="${VISIBILITY_TIMEOUT_MS:-5000}"
VISIBILITY_TARGET_MS="${VISIBILITY_TARGET_MS:-1000}"
EXTERNAL_RECALL_CMD="${EXTERNAL_RECALL_CMD:-}"
EMBEDNFS_RUSTC_WRAPPER="${EMBEDNFS_RUSTC_WRAPPER:-}"
SERVER_CARGO_TARGET_DIR="${SERVER_CARGO_TARGET_DIR:-${ARTIFACT_DIR}/target}"
NFS_VERSION="${NFS_VERSION:-4.1}"
MOUNT_OPTS="${MOUNT_OPTS:-vers=${NFS_VERSION},proto=tcp,port=${SERVER_PORT},sec=sys}"
SUDO="${SUDO:-sudo}"
STEP_TIMEOUT="${STEP_TIMEOUT:-45}"
MOUNT_TIMEOUT="${MOUNT_TIMEOUT:-30}"
SERVER_START_TIMEOUT="${SERVER_START_TIMEOUT:-120}"
SERVER_PID=""
MOUNT_ACTIVE=0
FAILURES=0

log() {
  printf '==> %s\n' "$*"
}

die() {
  printf 'error: %s\n' "$*" >&2
  exit 1
}

record() {
  local status="$1"
  local name="$2"
  local detail="${3:-}"
  printf '%s\t%s\t%s\n' "${status}" "${name}" "${detail}" | tee -a "${SUMMARY_FILE}" >/dev/null
}

need_cmd() {
  command -v "$1" >/dev/null 2>&1 || die "$1 is required but was not found"
}

is_mounted() {
  mountpoint -q "${MOUNT_DIR}" >/dev/null 2>&1
}

cleanup() {
  set +e
  if [[ "${MOUNT_ACTIVE}" == "1" ]] || is_mounted; then
    log "Unmounting ${MOUNT_DIR}"
    "${SUDO}" umount "${MOUNT_DIR}" >>"${ARTIFACT_DIR}/cleanup.log" 2>&1 || true
  fi
  stop_server
  if command -v dmesg >/dev/null 2>&1; then
    mkdir -p "${ARTIFACT_DIR}" >/dev/null 2>&1 || true
    dmesg >"${ARTIFACT_DIR}/dmesg-final.log" 2>/dev/null || true
  fi
}

stop_server() {
  local pids=()
  if [[ -n "${SERVER_PID}" ]]; then
    pids+=("${SERVER_PID}")
  fi
  if [[ -s "${SERVER_PID_FILE}" ]]; then
    local file_pid
    file_pid="$(cat "${SERVER_PID_FILE}" 2>/dev/null || true)"
    if [[ -n "${file_pid}" ]]; then
      pids+=("${file_pid}")
    fi
  fi

  local pid
  local stopped=()
  for pid in "${pids[@]}"; do
    if [[ " ${stopped[*]} " == *" ${pid} "* ]]; then
      continue
    fi
    stopped+=("${pid}")
    if kill -0 "${pid}" >/dev/null 2>&1; then
      log "Stopping server pid ${pid}"
      kill "${pid}" >>"${ARTIFACT_DIR}/cleanup.log" 2>&1 || true
      wait "${pid}" >>"${ARTIFACT_DIR}/cleanup.log" 2>&1 || true
    fi
  done
  rm -f "${SERVER_PID_FILE}" >/dev/null 2>&1 || true
}

run_step() {
  local name="$1"
  shift
  local safe_name="${name//[^A-Za-z0-9_.-]/_}"
  local log_file="${ARTIFACT_DIR}/${safe_name}.log"
  local pid=""
  local elapsed=0

  log "Running ${name}"
  (
    set -euo pipefail
    "$@"
  ) >"${log_file}" 2>&1 &
  pid=$!
  while kill -0 "${pid}" >/dev/null 2>&1; do
    if (( elapsed >= STEP_TIMEOUT )); then
      kill "${pid}" >/dev/null 2>&1 || true
      wait "${pid}" >/dev/null 2>&1 || true
      FAILURES=$((FAILURES + 1))
      record "FAIL" "${name}" "timeout=${STEP_TIMEOUT}s log=${log_file}"
      tail -n 80 "${log_file}" >&2 || true
      return
    fi
    sleep 1
    elapsed=$((elapsed + 1))
  done

  if wait "${pid}"; then
    record "PASS" "${name}" "${log_file}"
  else
    local rc=$?
    FAILURES=$((FAILURES + 1))
    record "FAIL" "${name}" "exit=${rc} log=${log_file}"
    tail -n 80 "${log_file}" >&2 || true
  fi
}

run_expected_fail_step() {
  local name="$1"
  local expected_pattern="$2"
  shift 2
  local safe_name="${name//[^A-Za-z0-9_.-]/_}"
  local log_file="${ARTIFACT_DIR}/${safe_name}.log"
  local pid=""
  local rc=0

  log "Running ${name}"
  (
    set -euo pipefail
    "$@"
  ) >"${log_file}" 2>&1 &
  pid=$!
  set +e
  wait "${pid}"
  rc=$?
  set -e

  if [[ "${rc}" == "0" ]]; then
    record "XPASS" "${name}" "${log_file}"
  elif grep -Eiq "${expected_pattern}" "${log_file}"; then
    record "XFAIL" "${name}" "expected failure log=${log_file}"
  else
    FAILURES=$((FAILURES + 1))
    record "FAIL" "${name}" "exit=${rc} log=${log_file}"
    tail -n 80 "${log_file}" >&2 || true
  fi
}

skip_step() {
  local name="$1"
  local reason="$2"
  record "SKIP" "${name}" "${reason}"
  log "Skipping ${name}: ${reason}"
}

wait_for_server() {
  local host="${SERVER_ADDR%:*}"
  local port="${SERVER_ADDR##*:}"
  for _ in $(seq 1 "$((SERVER_START_TIMEOUT * 10))"); do
    if [[ -n "${SERVER_PID}" ]] && ! kill -0 "${SERVER_PID}" >/dev/null 2>&1; then
      return 1
    fi
    if timeout 1 bash -c ":</dev/tcp/${host}/${port}" >/dev/null 2>&1; then
      return 0
    fi
    sleep 0.1
  done
  return 1
}

port_is_open() {
  local host="${SERVER_ADDR%:*}"
  local port="${SERVER_ADDR##*:}"
  timeout 1 bash -c ":</dev/tcp/${host}/${port}" >/dev/null 2>&1
}

wait_for_port_close() {
  for _ in $(seq 1 50); do
    if ! port_is_open; then
      return 0
    fi
    sleep 0.1
  done
  return 1
}

start_server() {
  local control_listen=""
  if [[ "${PRODUCT_BEHAVIOR}" == "1" ]]; then
    control_listen="${CONTROL_ADDR}"
  fi

  (
    cd "${ROOT_DIR}"
    RUST_LOG="${SERVER_RUST_LOG}" \
      RUSTC_WRAPPER="${EMBEDNFS_RUSTC_WRAPPER}" \
      CARGO_TARGET_DIR="${SERVER_CARGO_TARGET_DIR}" \
      EMBEDNFS_ROOT="${BACKING_DIR}" \
      EMBEDNFS_LISTEN="${SERVER_ADDR}" \
      EMBEDNFS_CONTROL_LISTEN="${control_listen}" \
      EMBEDNFS_DIRECTORY_DELEGATIONS="${DIRECTORY_DELEGATIONS}" \
      EMBEDNFS_RECALL_TIMEOUT_MS="${RECALL_TIMEOUT_MS}" \
      bash -lc "${SERVER_CMD}"
  ) >>"${SERVER_LOG}" 2>&1 &
  SERVER_PID=$!
  printf '%s\n' "${SERVER_PID}" >"${SERVER_PID_FILE}"
}

count_server_log() {
  local pattern="$1"
  grep -Ec "${pattern}" "${SERVER_LOG}" 2>/dev/null || true
}

count_metric() {
  local metric="$1"
  count_server_log "metric=${metric}([^A-Za-z0-9_]|$)"
}

percentile() {
  local percentile="$1"
  shift
  if (( "$#" == 0 )); then
    printf 'NA\n'
    return
  fi
  printf '%s\n' "$@" | sort -n | awk -v p="${percentile}" '
    { values[NR] = $1 }
    END {
      idx = int((p * NR + 99) / 100)
      if (idx < 1) {
        idx = 1
      }
      if (idx > NR) {
        idx = NR
      }
      print values[idx]
    }
  '
}

metric_recall_wait_percentile() {
  local percentile="$1"
  mapfile -t waits < <(sed -n 's/.*metric=recall_wait_ms value=\([0-9][0-9]*\).*/\1/p' "${SERVER_LOG}" 2>/dev/null || true)
  percentile "${percentile}" "${waits[@]}"
}

probe_delegation_trace() {
  local create_session_backchannel_ok
  local bind_conn_to_session_backchannel_ok
  local get_dir_delegation_seen
  local get_dir_delegation_ok
  local cb_recall_sent
  local cb_recall_ok
  local delegreturn_seen
  local recall_wait_ms_p50
  local recall_wait_ms_p95
  local recall_timeout_count
  local revocation_count

  create_session_backchannel_ok="$(count_metric 'create_session_backchannel_ok')"
  bind_conn_to_session_backchannel_ok="$(count_metric 'bind_conn_to_session_backchannel_ok')"
  get_dir_delegation_seen="$(count_metric 'get_dir_delegation_seen')"
  get_dir_delegation_ok="$(count_metric 'get_dir_delegation_ok')"
  cb_recall_sent="$(count_metric 'cb_recall_sent')"
  cb_recall_ok="$(count_metric 'cb_recall_ok')"
  delegreturn_seen="$(count_metric 'delegreturn_seen')"
  recall_wait_ms_p50="$(metric_recall_wait_percentile 50)"
  recall_wait_ms_p95="$(metric_recall_wait_percentile 95)"
  recall_timeout_count="$(count_metric 'recall_timeout')"
  revocation_count="$(count_metric 'revocation_count')"

  {
    printf 'kernel=%s\n' "$(uname -r)"
    printf 'mount_opts=%s\n' "${MOUNT_OPTS}"
    printf 'directory_delegations=%s\n' "${DIRECTORY_DELEGATIONS}"
    printf 'require_delegations=%s\n' "${REQUIRE_DELEGATIONS}"
    printf 'product_behavior=%s\n' "${PRODUCT_BEHAVIOR}"
    printf 'recall_timeout_ms=%s\n' "${RECALL_TIMEOUT_MS}"
    printf 'server_log=%s\n' "${SERVER_LOG}"
    printf 'create_session_backchannel_ok=%s\n' "${create_session_backchannel_ok}"
    printf 'bind_conn_to_session_backchannel_ok=%s\n' "${bind_conn_to_session_backchannel_ok}"
    printf 'get_dir_delegation_seen=%s\n' "${get_dir_delegation_seen}"
    printf 'get_dir_delegation_ok=%s\n' "${get_dir_delegation_ok}"
    printf 'cb_recall_sent=%s\n' "${cb_recall_sent}"
    printf 'cb_recall_ok=%s\n' "${cb_recall_ok}"
    printf 'delegreturn_seen=%s\n' "${delegreturn_seen}"
    printf 'recall_wait_ms_p50=%s\n' "${recall_wait_ms_p50}"
    printf 'recall_wait_ms_p95=%s\n' "${recall_wait_ms_p95}"
    printf 'recall_timeout_count=%s\n' "${recall_timeout_count}"
    printf 'revocation_count=%s\n' "${revocation_count}"
    if [[ -d /sys/module/nfs/parameters ]]; then
      printf '\n[nfs module parameters]\n'
      for param in /sys/module/nfs/parameters/*; do
        printf '%s=%s\n' "$(basename "${param}")" "$(cat "${param}" 2>/dev/null || true)"
      done
    fi
  }

  record "INFO" "delegation-trace" \
    "create_session_backchannel_ok=${create_session_backchannel_ok} bind_conn_to_session_backchannel_ok=${bind_conn_to_session_backchannel_ok} get_dir_delegation_seen=${get_dir_delegation_seen} get_dir_delegation_ok=${get_dir_delegation_ok} cb_recall_sent=${cb_recall_sent} cb_recall_ok=${cb_recall_ok} delegreturn_seen=${delegreturn_seen} recall_wait_ms_p50=${recall_wait_ms_p50} recall_wait_ms_p95=${recall_wait_ms_p95} recall_timeout_count=${recall_timeout_count} revocation_count=${revocation_count}"

  if [[ "${REQUIRE_DELEGATIONS}" == "1" ]]; then
    local gate_failures=0
    if [[ "${DIRECTORY_DELEGATIONS}" != "1" ]]; then
      echo "REQUIRE_DELEGATIONS=1 requires DIRECTORY_DELEGATIONS=1"
      gate_failures=$((gate_failures + 1))
    fi
    if (( create_session_backchannel_ok + bind_conn_to_session_backchannel_ok == 0 )); then
      echo "delegation gate failed: no usable backchannel was negotiated"
      gate_failures=$((gate_failures + 1))
    fi
    if (( get_dir_delegation_seen == 0 )); then
      echo "delegation gate failed: client never sent GET_DIR_DELEGATION"
      gate_failures=$((gate_failures + 1))
    fi
    if (( get_dir_delegation_ok == 0 )); then
      echo "delegation gate failed: server never returned GDD4_OK"
      gate_failures=$((gate_failures + 1))
    fi
    if (( cb_recall_sent == 0 )); then
      echo "delegation gate failed: server never sent CB_RECALL"
      gate_failures=$((gate_failures + 1))
    fi
    if (( cb_recall_ok == 0 )); then
      echo "delegation gate failed: client never acknowledged CB_RECALL successfully"
      gate_failures=$((gate_failures + 1))
    fi
    if (( delegreturn_seen == 0 )); then
      echo "delegation gate failed: client never sent DELEGRETURN"
      gate_failures=$((gate_failures + 1))
    fi
    if (( recall_timeout_count != 0 )); then
      echo "delegation gate failed: recall timeout count is ${recall_timeout_count}"
      gate_failures=$((gate_failures + 1))
    fi
    if (( revocation_count != 0 )); then
      echo "delegation gate failed: revocation count is ${revocation_count}"
      gate_failures=$((gate_failures + 1))
    fi
    if [[ "${recall_wait_ms_p95}" != "NA" ]] && (( recall_wait_ms_p95 >= RECALL_TIMEOUT_MS )); then
      echo "delegation gate failed: recall p95 ${recall_wait_ms_p95}ms reached recall timeout ${RECALL_TIMEOUT_MS}ms"
      gate_failures=$((gate_failures + 1))
    fi
    if (( gate_failures != 0 )); then
      return 1
    fi
  fi
}

now_ms() {
  date +%s%3N
}

wait_for_path_state() {
  local path="$1"
  local state="$2"
  local timeout_ms="$3"
  local start_ms
  local now
  start_ms="$(now_ms)"
  while true; do
    if [[ "${state}" == "present" && -e "${path}" ]]; then
      now="$(now_ms)"
      printf '%s\n' "$((now - start_ms))"
      return 0
    fi
    if [[ "${state}" == "absent" && ! -e "${path}" ]]; then
      now="$(now_ms)"
      printf '%s\n' "$((now - start_ms))"
      return 0
    fi
    now="$(now_ms)"
    if (( now - start_ms >= timeout_ms )); then
      printf '%s\n' "$((now - start_ms))"
      return 1
    fi
    sleep 0.025
  done
}

probe_delegation_product_behavior() {
  local lookup_before
  local getattr_before
  local lookup_after
  local getattr_after
  local missing="${MOUNT_DIR}/missing-negative-probe"
  local create_name="external-create-$$"
  local unlink_name="external-unlink-$$"
  local rename_from="external-rename-from-$$"
  local rename_to="external-rename-to-$$"
  local create_ms
  local unlink_ms
  local rename_ms

  lookup_before="$(count_server_log 'COMPOUND:.*"LOOKUP"')"
  getattr_before="$(count_server_log 'COMPOUND:.*"GETATTR"')"

  for _ in $(seq 1 100); do
    test ! -e "${missing}"
  done
  for _ in $(seq 1 20); do
    stat "${MOUNT_DIR}" >/dev/null
  done

  lookup_after="$(count_server_log 'COMPOUND:.*"LOOKUP"')"
  getattr_after="$(count_server_log 'COMPOUND:.*"GETATTR"')"

  run_external_recall "before external create"
  printf 'external\n' >"${BACKING_DIR}/${create_name}"
  create_ms="$(wait_for_path_state "${MOUNT_DIR}/${create_name}" present "${VISIBILITY_TIMEOUT_MS}")"
  test -f "${MOUNT_DIR}/${create_name}"

  run_external_recall "before external unlink setup create"
  printf 'external\n' >"${BACKING_DIR}/${unlink_name}"
  wait_for_path_state "${MOUNT_DIR}/${unlink_name}" present "${VISIBILITY_TIMEOUT_MS}" >/dev/null
  test -f "${MOUNT_DIR}/${unlink_name}"
  run_external_recall "before external unlink"
  rm "${BACKING_DIR}/${unlink_name}"
  unlink_ms="$(wait_for_path_state "${MOUNT_DIR}/${unlink_name}" absent "${VISIBILITY_TIMEOUT_MS}")"
  test ! -e "${MOUNT_DIR}/${unlink_name}"

  run_external_recall "before external rename setup create"
  printf 'external\n' >"${BACKING_DIR}/${rename_from}"
  wait_for_path_state "${MOUNT_DIR}/${rename_from}" present "${VISIBILITY_TIMEOUT_MS}" >/dev/null
  test -f "${MOUNT_DIR}/${rename_from}"
  run_external_recall "before external rename"
  mv "${BACKING_DIR}/${rename_from}" "${BACKING_DIR}/${rename_to}"
  rename_ms="$(wait_for_path_state "${MOUNT_DIR}/${rename_to}" present "${VISIBILITY_TIMEOUT_MS}")"
  test -f "${MOUNT_DIR}/${rename_to}"
  test ! -e "${MOUNT_DIR}/${rename_from}"

  mkdir "${MOUNT_DIR}/client-deleg-dir"
  printf 'client\n' >"${MOUNT_DIR}/client-deleg-dir/file.txt"
  mv "${MOUNT_DIR}/client-deleg-dir/file.txt" "${MOUNT_DIR}/client-deleg-dir/file-renamed.txt"
  rm "${MOUNT_DIR}/client-deleg-dir/file-renamed.txt"
  rmdir "${MOUNT_DIR}/client-deleg-dir"

  {
    printf 'negative_lookup_probe_count=100\n'
    printf 'lookup_delta=%s\n' "$((lookup_after - lookup_before))"
    printf 'getattr_delta=%s\n' "$((getattr_after - getattr_before))"
    printf 'external_create_visibility_ms=%s\n' "${create_ms}"
    printf 'external_unlink_visibility_ms=%s\n' "${unlink_ms}"
    printf 'external_rename_visibility_ms=%s\n' "${rename_ms}"
    printf 'visibility_target_ms=%s\n' "${VISIBILITY_TARGET_MS}"
    printf 'visibility_timeout_ms=%s\n' "${VISIBILITY_TIMEOUT_MS}"
  }

  record "INFO" "delegation-product-counters" \
    "lookup_delta=$((lookup_after - lookup_before)) getattr_delta=$((getattr_after - getattr_before)) create_ms=${create_ms} unlink_ms=${unlink_ms} rename_ms=${rename_ms} target_ms=${VISIBILITY_TARGET_MS}"

  if (( create_ms > VISIBILITY_TARGET_MS )); then
    echo "product gate failed: external create visibility ${create_ms}ms exceeded target ${VISIBILITY_TARGET_MS}ms"
    return 1
  fi
  if (( unlink_ms > VISIBILITY_TARGET_MS )); then
    echo "product gate failed: external unlink visibility ${unlink_ms}ms exceeded target ${VISIBILITY_TARGET_MS}ms"
    return 1
  fi
  if (( rename_ms > VISIBILITY_TARGET_MS )); then
    echo "product gate failed: external rename visibility ${rename_ms}ms exceeded target ${VISIBILITY_TARGET_MS}ms"
    return 1
  fi
}

run_external_recall() {
  local reason="$1"
  log "Running external recall hook: ${reason}"
  if [[ -n "${EXTERNAL_RECALL_CMD}" ]]; then
    BACKING_DIR="${BACKING_DIR}" \
      MOUNT_DIR="${MOUNT_DIR}" \
      SERVER_ADDR="${SERVER_ADDR}" \
      CONTROL_ADDR="${CONTROL_ADDR}" \
      bash -lc "${EXTERNAL_RECALL_CMD}"
    return
  fi

  local host="${CONTROL_ADDR%:*}"
  local port="${CONTROL_ADDR##*:}"
  local response=""
  response="$(
    timeout 5 bash -c \
      "exec 3<>/dev/tcp/${host}/${port}; printf 'RECALL /\\n' >&3; IFS= read -r response <&3; printf '%s\\n' \"\${response}\""
  )"
  if [[ "${response}" != "OK" ]]; then
    echo "control recall failed: ${response}"
    return 1
  fi
}

probe_mount_metadata() {
  findmnt --target "${MOUNT_DIR}"
  nfsstat -m || true
  stat "${MOUNT_DIR}"
  ls -la "${MOUNT_DIR}"
  find "${MOUNT_DIR}" -maxdepth 1 -ls
}

probe_basic_io() {
  set -x
  local file="${MOUNT_DIR}/hello.txt"
  local dir="${MOUNT_DIR}/dir"
  local moved="${dir}/moved.txt"

  printf 'hello linux nfs\n' >"${file}"
  grep -q '^hello linux nfs$' "${file}"
  mkdir "${dir}"
  ls -la "${MOUNT_DIR}" "${dir}"
  mv "${file}" "${moved}"
  ls -la "${MOUNT_DIR}" "${dir}"
  stat "${moved}"
  test -f "${moved}"
  cp "${moved}" "${MOUNT_DIR}/copy.txt"
  cmp "${moved}" "${MOUNT_DIR}/copy.txt"
  rm "${moved}" "${MOUNT_DIR}/copy.txt"
  rmdir "${dir}"
}

probe_large_io() {
  local src="${ARTIFACT_DIR}/large-src.bin"
  local dst="${MOUNT_DIR}/large.bin"
  local roundtrip="${ARTIFACT_DIR}/large-roundtrip.bin"

  dd if=/dev/urandom of="${src}" bs=1M count=4 status=none
  cp "${src}" "${dst}"
  sync "${dst}" || sync
  cp "${dst}" "${roundtrip}"
  cmp "${src}" "${roundtrip}"
  truncate -s 1048577 "${dst}"
  test "$(stat -c '%s' "${dst}")" = "1048577"
  rm "${dst}"
}

probe_metadata() {
  local file="${MOUNT_DIR}/meta.txt"
  printf 'metadata\n' >"${file}"
  chmod 0640 "${file}"
  test "$(stat -c '%a' "${file}")" = "640"
  touch -d '2024-01-02 03:04:05 UTC' "${file}"
  stat -c '%n mode=%a uid=%u gid=%g size=%s atime=%X mtime=%Y ctime=%Z birth=%W' "${file}"

  if [[ "$(id -u)" == "0" ]]; then
    chown 0:0 "${file}"
    test "$(stat -c '%u:%g' "${file}")" = "0:0"
  else
    echo "not root; chown check skipped"
  fi

  rm "${file}"
}

probe_links() {
  local file="${MOUNT_DIR}/links.txt"
  printf 'links\n' >"${file}"
  ln "${file}" "${MOUNT_DIR}/links-hard.txt"
  ln -s "links.txt" "${MOUNT_DIR}/links-sym.txt"
  test "$(cat "${MOUNT_DIR}/links-hard.txt")" = "links"
  test "$(readlink "${MOUNT_DIR}/links-sym.txt")" = "links.txt"
  stat -c '%n inode=%i links=%h type=%F' "${file}" "${MOUNT_DIR}/links-hard.txt" "${MOUNT_DIR}/links-sym.txt"
  rm "${MOUNT_DIR}/links-sym.txt" "${MOUNT_DIR}/links-hard.txt" "${file}"
}

probe_locks() {
  local file="${MOUNT_DIR}/lock.txt"
  printf 'lock\n' >"${file}"
  python3 - "${file}" <<'PY'
import fcntl
import os
import sys
import time

path = sys.argv[1]
r, w = os.pipe()
pid = os.fork()
if pid == 0:
    os.close(r)
    with open(path, "r+") as f:
        fcntl.lockf(f, fcntl.LOCK_EX)
        os.write(w, b"1")
        time.sleep(2)
    os._exit(0)

os.close(w)
os.read(r, 1)
with open(path, "r+") as f:
    try:
        fcntl.lockf(f, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        pass
    else:
        raise SystemExit("second lock unexpectedly succeeded")
os.waitpid(pid, 0)
PY
  rm "${file}"
}

probe_xattrs() {
  local file="${MOUNT_DIR}/xattr.txt"
  printf 'xattr\n' >"${file}"
  setfattr -n user.embednfs_probe -v value "${file}"
  getfattr -n user.embednfs_probe "${file}"
  rm "${file}"
}

probe_recovery_restart() {
  local file="${MOUNT_DIR}/before-restart.txt"
  printf 'before restart\n' >"${file}"

  kill "${SERVER_PID}"
  wait "${SERVER_PID}" >/dev/null 2>&1 || true
  SERVER_PID=""
  wait_for_port_close
  sleep 1

  start_server
  wait_for_server

  cat "${file}"
  printf 'after restart\n' >"${MOUNT_DIR}/after-restart.txt"
  cat "${MOUNT_DIR}/after-restart.txt"
}

trap cleanup EXIT

[[ "$(uname -s)" == "Linux" ]] || die "this harness must run inside the Linux VM"
need_cmd cargo
need_cmd findmnt
need_cmd mount
need_cmd mountpoint
need_cmd nfsstat
need_cmd ss
need_cmd stat
need_cmd timeout
need_cmd python3

mkdir -p "${ARTIFACT_DIR}" "${BACKING_DIR}" "${MOUNT_DIR}"
: >"${SUMMARY_FILE}"
: >"${SERVER_LOG}"

if is_mounted; then
  die "${MOUNT_DIR} is already mounted"
fi
if port_is_open; then
  ss -ltnp "sport = :${SERVER_PORT}" >&2 || true
  die "${SERVER_ADDR} is already listening; stop the existing server or set SERVER_PORT"
fi

log "Artifacts: ${ARTIFACT_DIR}"
log "Backing directory: ${BACKING_DIR}"
log "Mount directory: ${MOUNT_DIR}"
log "NFS version: ${NFS_VERSION}"
log "Directory delegations: ${DIRECTORY_DELEGATIONS}"
log "Require delegations: ${REQUIRE_DELEGATIONS}"
log "Product behavior probe: ${PRODUCT_BEHAVIOR}"
if [[ "${PRODUCT_BEHAVIOR}" == "1" ]]; then
  log "Control address: ${CONTROL_ADDR}"
fi
log "Starting server on ${SERVER_ADDR}"
start_server

if ! wait_for_server; then
  tail -n 120 "${SERVER_LOG}" >&2 || true
  die "server did not listen on ${SERVER_ADDR}"
fi

log "Mounting 127.0.0.1:/ with ${MOUNT_OPTS}"
timeout "${MOUNT_TIMEOUT}" "${SUDO}" mount -t nfs4 -o "${MOUNT_OPTS}" 127.0.0.1:/ "${MOUNT_DIR}"
MOUNT_ACTIVE=1

run_step "mount-metadata" probe_mount_metadata
run_step "basic-io" probe_basic_io
run_step "large-io" probe_large_io
run_step "metadata" probe_metadata
run_step "links" probe_links
run_step "locks" probe_locks

if command -v setfattr >/dev/null 2>&1 && command -v getfattr >/dev/null 2>&1; then
  if [[ "${NFS_VERSION}" == "4.2" ]]; then
    run_step "xattrs" probe_xattrs
  else
    run_expected_fail_step "xattrs" "Operation not supported|not supported" probe_xattrs
  fi
else
  skip_step "xattrs" "setfattr/getfattr not installed"
fi

run_step "server-restart-recovery" probe_recovery_restart
if [[ "${PRODUCT_BEHAVIOR}" == "1" ]]; then
  run_step "delegation-product-behavior" probe_delegation_product_behavior
fi
run_step "delegation-trace" probe_delegation_trace

log "Summary"
column -t -s $'\t' "${SUMMARY_FILE}" || cat "${SUMMARY_FILE}"
log "Server log: ${SERVER_LOG}"

if [[ "${FAILURES}" != "0" ]]; then
  exit 1
fi
