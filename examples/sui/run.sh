#!/bin/bash
# Build sui-node, stress and sui with rooms for the rewriter, make the
# cluster's genesis once, and run run.yaml under the supervisor.
#
#   examples/sui/run.sh [--regenesis] [rewrite run options...]
#
# SUI_DIR is the sui checkout (default ~/repos/sui), SCRATCH the run's
# directory (default examples/sui/scratch: host directories, stdout.N and
# stderr.N per process), SUI_CARGO_FLAGS extra flags for the build (such
# as --no-default-features, for the system allocator and a seeded heap).
# TIDEHUNTER=1 builds the nodes on tidehunter instead of RocksDB, into a
# target directory of their own: the two stores' databases are not
# compatible.
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
derp=$(cd "$here/../.." && pwd)
sui_dir=${SUI_DIR:-$HOME/repos/sui}
scratch=${SCRATCH:-$here/scratch}
python=/opt/homebrew/opt/python@3.13/bin/python3.13
venv=$derp/examples/.venv

regenesis=0
if [[ ${1:-} == --regenesis ]]; then
    regenesis=1
    shift
fi

(cd "$derp" && cargo build --release --workspace)
rewrite=$derp/target/release/rewrite

target=$sui_dir/target
flags=${SUI_CARGO_FLAGS:-}
if [[ ${TIDEHUNTER:-} == 1 ]]; then
    export USE_TIDEHUNTER=1
    target=$sui_dir/target/tidehunter
    flags="$flags --features typed-store/tidehunter"
fi
# shellcheck disable=SC2086
(cd "$sui_dir" && CARGO_TARGET_DIR=$target "$rewrite" cargo build --release $flags \
    --bin sui-node --bin stress --bin sui)
mkdir -p "$here/bin"
for prog in sui-node stress sui; do
    ln -sf "$target/release/$prog" "$here/bin/$prog"
done
ln -sfn "$sui_dir" "$here/sui-src"

# The genesis is made natively and kept: its keys are random, and a run
# is only repeatable with the same ones. Make it again after rebuilding
# sui at another commit.
if [[ $regenesis == 1 || ! -f $here/cluster/genesis.blob ]]; then
    if ! "$venv/bin/python" -c 'import yaml, cryptography' 2>/dev/null; then
        "$python" -m venv "$venv"
        "$venv/bin/pip" install --quiet pyyaml cryptography
    fi
    rm -rf "$here/cluster"
    "$venv/bin/python" "$here/genesis.py" "$here/bin/sui" "$here/cluster"
fi

exec "$rewrite" run --manifest "$here/run.yaml" --capture --capture-stderr \
    --scratch "$scratch" "$@"
