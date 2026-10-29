#!/usr/bin/env bash
# `npm ci` for the window's frontend that also works with the npm of Ubuntu
# 24.04-26.04 and Debian 13 (9.2.0). That npm cannot read the `deepmerge`
# override pointing at vendor/deepmerge: its arborist throws "Invalid
# comparator: file:../../../vendor/deepmerge" and reports it as a missing
# package-lock.json. npm 10 and newer install as they are; an older npm
# fetches npm 10 from the registry (the install needs the registry anyway)
# and that one installs.
#
#   cd crates/gui/frontend && ../../../scripts/npm-ci.sh [npm ci options]
#
# npm 10 is looked up from a folder outside the project: `npm exec` run
# inside it reads the project's tree with the old npm and fails the same way.
set -euo pipefail

major="$(npm --version | cut -d. -f1)"
if (( major >= 10 )); then
    exec npm ci "$@"
fi

npm10="$(cd "${TMPDIR:-/tmp}" && npm exec --yes --package=npm@10 -- sh -c 'command -v npm')"
if [[ -z "$npm10" ]] || ! "$npm10" --version | grep -q '^10\.'; then
    echo "npm $(npm --version) cannot install this frontend, and npm 10 could not be fetched" >&2
    exit 1
fi
echo "npm $(npm --version) is too old for this frontend; installing with npm $("$npm10" --version)" >&2
exec "$npm10" ci "$@"
