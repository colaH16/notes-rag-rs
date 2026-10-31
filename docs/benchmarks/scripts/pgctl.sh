#!/bin/sh
# Disposable loopback-only PostgreSQL 18 + pgvector for the synthetic benchmark.
# Usage: pgctl.sh start|stop|status   (BENCH=/tmp/rag-bench, PG_CPUS=2,3)
set -eu
BENCH=${BENCH:-/tmp/rag-bench}
PG_CPUS=${PG_CPUS:-2,3}
PGBIN=$BENCH/pg/bin
case "${1:-}" in
  start)
    # Postmaster and every backend inherit the CPU affinity.
    taskset -c "$PG_CPUS" "$PGBIN/pg_ctl" -D "$BENCH/pgdata" -l "$BENCH/run/pg.log" -w start ;;
  stop) "$PGBIN/pg_ctl" -D "$BENCH/pgdata" -m fast -w stop ;;
  status) "$PGBIN/pg_ctl" -D "$BENCH/pgdata" status ;;
  *) echo "usage: $0 start|stop|status" >&2; exit 2 ;;
esac
