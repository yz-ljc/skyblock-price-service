#!/bin/sh
set -eu
umask 077

case "${1:-}" in
    -h|--help)
        printf 'Usage: sh run.sh\nRuns in the foreground. Ctrl+C stops the server.\n'
        exit 0
        ;;
esac
[ "$#" -eq 0 ] || { printf 'Usage: sh run.sh\n' >&2; exit 1; }

app_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
cd -- "$app_dir"
[ -x ./skyblock-price-service ] || {
    printf 'Missing executable: extract the Linux binary package first.\n' >&2
    exit 1
}

# A generated token survives restarts and archive upgrades. An explicitly
# supplied environment token takes precedence over the local .env file.
if [ -z "${PRICE_API_TOKEN:-}" ]; then
    if [ ! -f .env ]; then
        token=$(od -An -N32 -tx1 /dev/urandom | tr -d ' \n')
        [ "${#token}" -eq 64 ] || { printf 'Could not generate access token.\n' >&2; exit 1; }
        (set -C; printf 'PRICE_API_TOKEN=%s\n' "$token" > .env)
        unset token
        printf 'Created .env with a persistent access token. Read it with: cat .env\n'
    fi
    set -a
    . "${app_dir}/.env"
    set +a
fi

if [ -z "${PRICE_CONFIG:-}" ]; then
    [ -f config.toml ] || cp config.example.toml config.toml
    PRICE_CONFIG="$app_dir/config.toml"
fi
export PRICE_CONFIG
printf 'Config: %s\nRunning in foreground; Ctrl+C to stop.\n' "$PRICE_CONFIG"
# Replace the shell so screen and terminal signals reach the Rust process.
exec "$app_dir/skyblock-price-service"
