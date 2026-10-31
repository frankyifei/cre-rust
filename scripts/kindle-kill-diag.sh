#!/bin/sh
# Collect evidence for a `Killed` (SIGKILL) crash of KOReader on a Kindle.
#
# Run this ON the device (it is a POSIX sh script, not a host script):
#
#     cd /mnt/us/koreader
#     sh kindle-kill-diag.sh                     # open the last book
#     sh kindle-kill-diag.sh /mnt/us/documents/x.epub
#
# It records the state before the run, launches KOReader with cre tracing
# enabled, and afterwards appends everything that can tell SIGKILL causes
# apart: the kernel OOM counters (/proc/vmstat), dmesg, the cre breadcrumb
# (cre.log) and crash.log. Everything lands in one file, so a single
# `cat` of it is enough to diagnose the kill.
#
# Why /proc/vmstat: `oom_kill` increments for every task the kernel killed
# for memory reasons, and it is readable without log access. If it does not
# move across a crash, the kill did not come from the OOM killer.

set -u

DIR="$(cd "$(dirname "$0")" 2>/dev/null && pwd -P)"
OUT="diag-$(date +%Y%m%d-%H%M%S).txt"

log() {
    echo "$@" >>"$OUT"
}

oom_counters() {
    # Older kernels (Kindle PW2 is 3.0/3.8-ish) may not have all of these,
    # and some Amazon kernels ship an Android-style low memory killer.
    grep -iE 'oom|lowmem|pgscan_direct' /proc/vmstat 2>/dev/null
}

process_table() {
    # Who holds the memory, and which daemons could be sending the SIGKILL.
    ps 2>/dev/null | head -n 40
}

section() {
    log ""
    log "===== $* ====="
}

section "run $(date)"
log "kernel: $(uname -a)"
log "module: $(ls -l libs/libkoreader-cre.so 2>/dev/null)"
log "overcommit_memory: $(cat /proc/sys/vm/overcommit_memory 2>/dev/null)"
log "--- /proc/meminfo (before) ---"
grep -E '^(MemTotal|MemFree|MemAvailable|Buffers|Cached|SwapTotal|CommitLimit|Committed_AS|Slab):' /proc/meminfo >>"$OUT" 2>/dev/null
log "--- free (before) ---"
free >>"$OUT" 2>&1
log "--- vmstat oom counters (before) ---"
oom_counters >>"$OUT"
log "--- processes (before) ---"
process_table >>"$OUT"
log "--- init ---"
cat /proc/1/cmdline 2>/dev/null | tr '\0' ' ' >>"$OUT"
log "" 

# Keep the previous logs readable: the crash appends to them.
if [ -f crash.log ]; then
    cp -f crash.log crash.log.before
fi

section "launch"
CRE_LIB_TRACE=1 \
CRE_LUA_TRACE_PATH="$DIR/cre.log" \
    ./koreader.sh "$@" >>"$OUT" 2>&1
status=$?
log "koreader.sh exit status: ${status}"

section "after"
log "--- /proc/meminfo ---"
grep -E '^(MemTotal|MemFree|MemAvailable|Buffers|Cached|SwapTotal|CommitLimit|Committed_AS|Slab):' /proc/meminfo >>"$OUT" 2>/dev/null
log "--- free ---"
free >>"$OUT" 2>&1
log "--- vmstat oom counters ---"
oom_counters >>"$OUT"
log "--- processes ---"
process_table >>"$OUT"
log "--- dmesg tail ---"
dmesg 2>/dev/null | tail -n 40 >>"$OUT"
log "--- /var/log/messages tail ---"
tail -n 40 /var/log/messages 2>/dev/null >>"$OUT"

section "crash.log (new part)"
if [ -f crash.log.before ]; then
    before_lines=$(wc -l <crash.log.before)
    tail -n +$((before_lines + 1)) crash.log 2>/dev/null | tail -n 60 >>"$OUT"
else
    tail -n 60 crash.log 2>/dev/null >>"$OUT"
fi

section "cre.log (breadcrumb tail)"
tail -n 30 cre.log 2>/dev/null >>"$OUT"

echo "wrote $DIR/$OUT"
