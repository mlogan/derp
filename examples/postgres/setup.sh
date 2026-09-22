#!/bin/sh
# Once, natively: a PostgreSQL data directory the run copies into the
# server's host directory (initdb cannot run as a guest: it shells out to
# /bin/sh, which is Apple's). Needs Homebrew's postgresql@17.
set -e
cd "$(dirname "$0")"
PG=${PG:-/opt/homebrew/opt/postgresql@17/bin}
rm -rf data
"$PG/initdb" -D data -U guest --auth=trust --no-locale -E UTF8 > initdb.log
echo "host all all 10.0.0.0/8 trust" >> data/pg_hba.conf
echo "data directory ready"
