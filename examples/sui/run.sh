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

(cd "$derp" && cargo build --release)
rewrite=$derp/target/release/rewrite

# shellcheck disable=SC2086
(cd "$sui_dir" && "$rewrite" cargo build --release ${SUI_CARGO_FLAGS:-} \
    --bin sui-node --bin stress --bin sui)
mkdir -p "$here/bin"
for prog in sui-node stress sui; do
    ln -sf "$sui_dir/target/release/$prog" "$here/bin/$prog"
done

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
