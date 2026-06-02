#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RUN_ID="$(date +%Y%m%d-%H%M%S)"
ARTIFACT_DIR="${ARTIFACT_DIR:-/tmp/embednfs-linux-smoke-${RUN_ID}}"
BACKING_DIR="${BACKING_DIR:-${ARTIFACT_DIR}/backing}"
MOUNT_DIR="${MOUNT_DIR:-${ARTIFACT_DIR}/mnt}"
SERVER_LOG="${SERVER_LOG:-${ARTIFACT_DIR}/server.log}"
SUMMARY_FILE="${SUMMARY_FILE:-${ARTIFACT_DIR}/summary.tsv}"
SERVER_PORT="${SERVER_PORT:-12049}"
SERVER_ADDR="${SERVER_ADDR:-127.0.0.1:${SERVER_PORT}}"
SERVER_CMD="${SERVER_CMD:-cargo run -p embednfsd --release}"
SERVER_RUST_LOG="${SERVER_RUST_LOG:-embednfs=debug,embednfsd=info}"
DIRECTORY_DELEGATIONS="${DIRECTORY_DELEGATIONS:-0}"
RECALL_TIMEOUT_MS="${RECALL_TIMEOUT_MS:-1000}"
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
  if [[ -n "${SERVER_PID}" ]]; then
    log "Stopping server pid ${SERVER_PID}"
    kill "${SERVER_PID}" >>"${ARTIFACT_DIR}/cleanup.log" 2>&1 || true
    wait "${SERVER_PID}" >>"${ARTIFACT_DIR}/cleanup.log" 2>&1 || true
  fi
  if command -v dmesg >/dev/null 2>&1; then
    mkdir -p "${ARTIFACT_DIR}" >/dev/null 2>&1 || true
    dmesg >"${ARTIFACT_DIR}/dmesg-final.log" 2>/dev/null || true
  fi
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
  (
    cd "${ROOT_DIR}"
    RUST_LOG="${SERVER_RUST_LOG}" \
      RUSTC_WRAPPER="${EMBEDNFS_RUSTC_WRAPPER}" \
      CARGO_TARGET_DIR="${SERVER_CARGO_TARGET_DIR}" \
      EMBEDNFS_ROOT="${BACKING_DIR}" \
      EMBEDNFS_LISTEN="${SERVER_ADDR}" \
      EMBEDNFS_DIRECTORY_DELEGATIONS="${DIRECTORY_DELEGATIONS}" \
      EMBEDNFS_RECALL_TIMEOUT_MS="${RECALL_TIMEOUT_MS}" \
      bash -lc "${SERVER_CMD}"
  ) >>"${SERVER_LOG}" 2>&1 &
  SERVER_PID=$!
}

count_server_log() {
  local pattern="$1"
  grep -Ec "${pattern}" "${SERVER_LOG}" 2>/dev/null || true
}

probe_delegation_trace() {
  local get_dir_count
  local get_dir_ok_count
  local recall_count
  local backchannel_ctl_count

  get_dir_count="$(count_server_log 'GET_DIR_DELEGATION')"
  get_dir_ok_count="$(count_server_log 'result: op=GET_DIR_DELEGATION, status=Ok')"
  recall_count="$(count_server_log 'CB_RECALL|op=DELEGRETURN')"
  backchannel_ctl_count="$(count_server_log 'BACKCHANNEL_CTL')"

  {
    printf 'kernel=%s\n' "$(uname -r)"
    printf 'mount_opts=%s\n' "${MOUNT_OPTS}"
    printf 'directory_delegations=%s\n' "${DIRECTORY_DELEGATIONS}"
    printf 'recall_timeout_ms=%s\n' "${RECALL_TIMEOUT_MS}"
    printf 'server_log=%s\n' "${SERVER_LOG}"
    printf 'get_dir_delegation_lines=%s\n' "${get_dir_count}"
    printf 'get_dir_delegation_ok_lines=%s\n' "${get_dir_ok_count}"
    printf 'recall_or_delegreturn_lines=%s\n' "${recall_count}"
    printf 'backchannel_ctl_lines=%s\n' "${backchannel_ctl_count}"
    if [[ -d /sys/module/nfs/parameters ]]; then
      printf '\n[nfs module parameters]\n'
      for param in /sys/module/nfs/parameters/*; do
        printf '%s=%s\n' "$(basename "${param}")" "$(cat "${param}" 2>/dev/null || true)"
      done
    fi
  }

  record "INFO" "delegation-trace" \
    "gdd=${get_dir_count} gdd_ok=${get_dir_ok_count} recall=${recall_count} backchannel_ctl=${backchannel_ctl_count}"
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
run_step "delegation-trace" probe_delegation_trace

log "Summary"
column -t -s $'\t' "${SUMMARY_FILE}" || cat "${SUMMARY_FILE}"
log "Server log: ${SERVER_LOG}"

if [[ "${FAILURES}" != "0" ]]; then
  exit 1
fi
