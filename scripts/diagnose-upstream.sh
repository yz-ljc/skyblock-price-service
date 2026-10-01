#!/usr/bin/env bash
# Read-only network probe: two public pages, no API key or service token.
set -u
command -v curl >/dev/null || { echo 'curl is required'; exit 1; }
probe_dir=$(mktemp -d /tmp/skyblock-price-probe.XXXXXXXX) || exit 1
cleanup() {
    rm -f -- "$probe_dir/headers" "$probe_dir/body"
    rmdir -- "$probe_dir"
}
trap cleanup EXIT
printf 'UTC: '; date -u '+%Y-%m-%d %H:%M:%S'
printf 'Architecture: '; uname -m
printf '\nPublic Hypixel auction download probe (at most 30 seconds per page)\n'
for page in 0 1; do
    printf '\nPage %s\n' "$page"
    : > "$probe_dir/headers"
    : > "$probe_dir/body"
    curl --silent --show-error --compressed --connect-timeout 5 --max-time 30 \
        --dump-header "$probe_dir/headers" --output "$probe_dir/body" \
        --write-out 'http=%{http_code} dns=%{time_namelookup}s connect=%{time_connect}s tls=%{time_appconnect}s first_byte=%{time_starttransfer}s total=%{time_total}s transferred=%{size_download} bytes speed=%{speed_download} bytes/s\n' \
        "https://api.hypixel.net/v2/skyblock/auctions?page=$page"
    printf 'curl_exit=%s\n' "$?"
    if [[ -f $probe_dir/headers ]]; then
        grep -Ei '^(HTTP/|content-encoding:|content-length:|age:|cf-cache-status:|cf-ray:|cache-control:)' "$probe_dir/headers" || true
    fi
    if [[ -f $probe_dir/body ]]; then
        printf 'decoded_bytes='; wc -c < "$probe_dir/body"
    fi
done
