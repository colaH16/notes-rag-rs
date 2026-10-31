#!/bin/sh
# Reproduce the benchmark preparation from scratch (no sudo, no system installs).
#   ORIGINAL=<path to Python notes-rag git repo> scripts/setup.sh
# Creates $BENCH (default /tmp/rag-bench) and $BENCH_VENV (default /tmp/rag-bench-venv).
set -eu
BENCH=${BENCH:-/tmp/rag-bench}
VENV=${BENCH_VENV:-/tmp/rag-bench-venv}
COMMIT=754a97398ac1d6ef313605f84eb7249db8eb556e
PG_VERSION=18.6
PG_SHA256=555610c24d53e4316da5b7d3fc25c279d96856d5e0e23ee308c328c5fa881d9f
PGVECTOR_VERSION=0.8.1
PGVECTOR_SHA256=a9094dfb85ccdde3cbb295f1086d4c71a20db1d26bf1d6c39f07a7d164033eb4
HERE=$(cd "$(dirname "$0")" && pwd)
PORT=55432

mkdir -p "$BENCH/oracle" "$BENCH/dl" "$BENCH/build" "$BENCH/run/pgsock"
chmod 700 "$BENCH/run"

# 1. Pinned Python oracle (read-only source -> scratch copy).
git -C "$ORIGINAL" archive --format=tar "$COMMIT" | tar -x -C "$BENCH/oracle"

# 2. Python 3.11 venv with the exact production lock (image: python 3.11.16; host 3.11.x).
python3.11 -m venv "$VENV"
"$VENV/bin/pip" install --disable-pip-version-check -q -r "$BENCH/oracle/requirements.lock"
"$VENV/bin/pip" check
"$VENV/bin/pip" freeze > "$BENCH/venv-freeze.txt"

# 3. Pinned public tokenizer via the oracle's own checksum-verifying downloader.
mkdir -p "$BENCH/tok"
(cd "$BENCH/oracle" && RAG_TOKENIZER_PATH="$BENCH/tok/qwen-tokenizer.json" "$VENV/bin/python" embedding_config.py --download-tokenizer)

# 4. PostgreSQL + pgvector from official sources, built into scratch.
cd "$BENCH/dl"
curl -sSfLO "https://ftp.postgresql.org/pub/source/v$PG_VERSION/postgresql-$PG_VERSION.tar.bz2"
echo "$PG_SHA256  postgresql-$PG_VERSION.tar.bz2" | sha256sum -c
curl -sSfL -o "pgvector-$PGVECTOR_VERSION.tar.gz" "https://github.com/pgvector/pgvector/archive/refs/tags/v$PGVECTOR_VERSION.tar.gz"
echo "$PGVECTOR_SHA256  pgvector-$PGVECTOR_VERSION.tar.gz" | sha256sum -c
cd "$BENCH/build"
python3 -I -c "import bz2,sys,shutil; shutil.copyfileobj(bz2.open(sys.argv[1]), open(sys.argv[2],'wb'))" \
  "$BENCH/dl/postgresql-$PG_VERSION.tar.bz2" "$BENCH/dl/postgresql-$PG_VERSION.tar"
tar xf "$BENCH/dl/postgresql-$PG_VERSION.tar"
tar xzf "$BENCH/dl/pgvector-$PGVECTOR_VERSION.tar.gz"
(cd "postgresql-$PG_VERSION" && ./configure --prefix="$BENCH/pg" --without-readline --without-icu --without-openssl \
   && make -j6 && make install) > "$BENCH/build/pg-build.log" 2>&1
# OPTFLAGS="" = generic build (no -march=native), like distribution packages.
(cd "pgvector-$PGVECTOR_VERSION" && make PG_CONFIG="$BENCH/pg/bin/pg_config" OPTFLAGS="" \
   && make PG_CONFIG="$BENCH/pg/bin/pg_config" install) > "$BENCH/build/pgvector-build.log" 2>&1

# 5. Test-only secrets (mode 0600) and a loopback-only cluster.
umask 077
for name in admin.pw reader.pw; do [ -f "$BENCH/run/$name" ] || python3 -c 'import secrets;print(secrets.token_urlsafe(24))' > "$BENCH/run/$name"; done
for name in mcp owner web_secret session embed_key; do [ -f "$BENCH/run/$name.tok" ] || python3 -c 'import secrets;print("bench-"+secrets.token_urlsafe(40))' > "$BENCH/run/$name.tok"; done
"$BENCH/pg/bin/initdb" -D "$BENCH/pgdata" -U bench_admin --pwfile="$BENCH/run/admin.pw" --auth=scram-sha-256 -E UTF8 --locale=C.UTF-8 > "$BENCH/run/initdb.log"
cat >> "$BENCH/pgdata/postgresql.conf" <<EOF
# rag-bench: isolated, loopback-only, synthetic data
listen_addresses = '127.0.0.1'
port = $PORT
unix_socket_directories = '$BENCH/run/pgsock'
max_connections = 60
shared_buffers = 256MB
work_mem = 16MB
timezone = 'UTC'
EOF
BENCH="$BENCH" "$HERE/pgctl.sh" start

# 6. Synthetic fixture + database (oracle schema files applied verbatim).
"$VENV/bin/python" "$HERE/gen_fixtures.py" --oracle "$BENCH/oracle" --tokenizer "$BENCH/tok/qwen-tokenizer.json" --out "$BENCH/fixture"
"$VENV/bin/python" "$HERE/load_db.py" --oracle "$BENCH/oracle" --fixture "$BENCH/fixture" \
  --admin-url "postgresql://bench_admin:$(cat "$BENCH/run/admin.pw")@127.0.0.1:$PORT/postgres" \
  --reader-password-file "$BENCH/run/reader.pw"
echo "setup complete"
