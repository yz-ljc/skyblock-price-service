#!/usr/bin/env bash
# Installs an already compiled Linux bundle. Runtime installation never invokes Cargo or rustup.
set -Eeuo pipefail
export LC_ALL=C
umask 077

bundle_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
install_dir=${PRICE_INSTALL_DIR:-/opt/skyblock-price-service}
config_file=${PRICE_DEPLOY_CONFIG_FILE:-/etc/skyblock-price-service.toml}
env_file=${PRICE_DEPLOY_ENV_FILE:-/etc/skyblock-price-service.env}
data_dir=${PRICE_DEPLOY_DATA_DIR:-/var/lib/skyblock-price-service}
stage=

help() {
    cat <<'HELP'
Usage: sudo bash deploy/install.sh

Requires: Ubuntu/Debian with systemd and a compiled Linux bundle.
The default listener is 0.0.0.0:25577, configured by bind in the TOML file.

The script installs the binary and systemd service. No remote build or firewall changes.
Repeat the command to update; existing token, configuration and snapshots survive.
Paths can be configured using PRICE_INSTALL_DIR, PRICE_DEPLOY_CONFIG_FILE,
PRICE_DEPLOY_ENV_FILE and PRICE_DEPLOY_DATA_DIR.
HELP
}
fail() { printf '\nERROR: %s\n' "$*" >&2; exit 1; }
cleanup() {
    if [[ $stage == /tmp/skyblock-price-install.* ]]; then
        rm -rf -- "$stage"
    fi
}
trap cleanup EXIT
trap 'printf "\nDeployment failed at line %s. Inspect: journalctl -u skyblock-price -n 50\n" "$LINENO" >&2' ERR

while (($#)); do
    case $1 in
        --help|-h) help; exit 0 ;;
        *) fail "Unknown argument: $1" ;;
    esac
done
[[ $(uname -s) == Linux ]] || fail 'Run this installer on the target Linux server.'
[[ $EUID == 0 ]] || fail 'Run with sudo bash deploy/install.sh.'
[[ -d /run/systemd/system ]] || fail 'This installer requires systemd.'
command -v apt-get >/dev/null || fail 'This installer supports Ubuntu/Debian.'
for path in "$install_dir" "$config_file" "$env_file" "$data_dir"; do
    [[ $path =~ ^/[a-zA-Z0-9_./-]+$ && $path != / && $path != */ ]] || fail "Invalid deployment path: $path"
    [[ /$path/ != */../* && /$path/ != */./* ]] || fail "Dot segments are not allowed: $path"
done
[[ $config_file != "$env_file" ]] || fail 'Config and token paths must be different.'
for file in skyblock-price-service config.example.toml THIRD_PARTY_NOTICES.txt SHA256SUMS; do
    [[ -f $bundle_dir/$file ]] || fail 'Use the compiled .tar.gz bundle, not the source directory. See docs/deployment.md.'
done
printf '[1/4] Verify bundle and CPU architecture\n'
(cd -- "$bundle_dir" && sha256sum --strict -c SHA256SUMS)
magic=$(od -An -tx1 -N6 "$bundle_dir/skyblock-price-service" | tr -d ' \n')
[[ $magic == 7f454c460201 ]] || fail 'Expected a 64-bit little-endian Linux ELF binary.'
machine=$(od -An -tx1 -j18 -N2 "$bundle_dir/skyblock-price-service" | tr -d ' \n')
case $(uname -m):$machine in
    x86_64:3e00|aarch64:b700|arm64:b700) ;;
    *) fail 'The bundle does not match this server CPU. Download the amd64 or arm64 bundle as appropriate.' ;;
esac
if [[ -f $config_file ]]; then
    existing_data=$(sed -n 's/^data_directory[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' "$config_file")
    [[ $existing_data == "$data_dir" ]] || fail "Existing data_directory differs; set PRICE_DEPLOY_DATA_DIR to that path. Configuration was preserved."
fi
bind_config="$bundle_dir/config.example.toml"
if [[ -f $config_file ]]; then bind_config=$config_file; fi
bind_address=$(sed -n 's/^bind[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' "$bind_config")
bind_address=${bind_address:-0.0.0.0:25577}
if [[ -f $env_file ]]; then
    bind_override=$(sed -n 's/^PRICE_BIND=//p' "$env_file")
    if [[ $bind_override == \"*\" || $bind_override == \'*\' ]]; then bind_override=${bind_override:1:-1}; fi
    bind_address=${bind_override:-$bind_address}
fi
[[ $bind_address =~ ^([0-9.]+|\[[0-9a-fA-F:]+\]):[0-9]+$ ]] || fail 'Invalid configured bind address.'
listen_port=${bind_address##*:}
((10#$listen_port >= 1 && 10#$listen_port <= 65535)) || fail 'Invalid configured port.'
probe_address=$bind_address
case $bind_address in
    0.0.0.0:*) probe_address="127.0.0.1:$listen_port" ;;
    '[::]:'*) probe_address="[::1]:$listen_port" ;;
esac

check_ports() {
    local service_pid sockets line
    service_pid=$(systemctl show skyblock-price.service -p MainPID --value 2>/dev/null || true)
    sockets=$(ss -H -ltnp "sport = :$listen_port")
    while IFS= read -r line; do
        [[ -z $line ]] && continue
        if [[ ${service_pid:-0} == 0 || $line != *"pid=$service_pid,"* ]]; then
            fail "TCP $listen_port is already used by another process."
        fi
    done <<< "$sockets"
}
if command -v ss >/dev/null; then check_ports; fi
stage=$(mktemp -d /tmp/skyblock-price-install.XXXXXXXX)
exec 9>/run/lock/skyblock-price-install.lock
flock -n 9 || fail 'Another deployment is running.'

printf '[2/4] Install runtime packages\n'
export DEBIAN_FRONTEND=noninteractive
apt-get update
apt-get install -y ca-certificates curl openssl iproute2 binutils
check_ports
if readelf -l "$bundle_dir/skyblock-price-service" | grep -q 'INTERP'; then
    fail 'Use the static musl bundle; this executable requires a dynamic loader.'
fi

printf '[3/4] Install binary; preserve configuration, token and cache\n'
if ! id skyblock-price >/dev/null 2>&1; then
    useradd --system --user-group --no-create-home --shell /usr/sbin/nologin skyblock-price
fi
install -d -m 755 "$install_dir" "$(dirname -- "$config_file")" "$(dirname -- "$env_file")"
install -d -o skyblock-price -g skyblock-price -m 750 "$data_dir"
if [[ ! -f $config_file ]]; then
    sed "s|^data_directory = .*|data_directory = \"$data_dir\"|" "$bundle_dir/config.example.toml" > "$stage/config.toml"
    install -o root -g skyblock-price -m 640 "$stage/config.toml" "$config_file"
fi
if [[ ! -f $env_file ]]; then
    printf 'PRICE_API_TOKEN=%s\n' "$(openssl rand -hex 32)" > "$stage/token.env"
    install -o root -g root -m 600 "$stage/token.env" "$env_file"
fi
token=$(sed -n 's/^PRICE_API_TOKEN=//p' "$env_file")
if [[ $token == \"*\" || $token == \'*\' ]]; then token=${token:1:-1}; fi
[[ ${#token} -ge 24 && ${#token} -le 256 && $token != *[!\ -~]* && $token != *[[:space:]]* ]] \
    || fail 'Existing PRICE_API_TOKEN is invalid; the token file was preserved.'
install -m 644 "$bundle_dir/THIRD_PARTY_NOTICES.txt" "$install_dir/THIRD_PARTY_NOTICES.txt"
install -m 755 "$bundle_dir/skyblock-price-service" "$install_dir/skyblock-price-service.next"
has_previous=false
if [[ -f $install_dir/skyblock-price-service ]]; then
    install -m 755 "$install_dir/skyblock-price-service" "$install_dir/skyblock-price-service.previous"
    has_previous=true
fi
mv -f -- "$install_dir/skyblock-price-service.next" "$install_dir/skyblock-price-service"

cat > "$stage/skyblock-price.service" <<UNIT
[Unit]
Description=AtriMeow SkyBlock price service
After=network-online.target
Wants=network-online.target
[Service]
Type=simple
User=skyblock-price
Group=skyblock-price
WorkingDirectory=$install_dir
EnvironmentFile=$env_file
ExecStart=/usr/bin/env PRICE_CONFIG=$config_file $install_dir/skyblock-price-service
Restart=on-failure
RestartSec=10
TimeoutStopSec=30
MemoryHigh=256M
MemoryMax=512M
TasksMax=32
LimitNOFILE=1024
NoNewPrivileges=true
PrivateTmp=true
ProtectSystem=strict
ProtectHome=true
ReadWritePaths=$data_dir
UMask=0077
[Install]
WantedBy=multi-user.target
UNIT

install -m 644 "$stage/skyblock-price.service" /etc/systemd/system/
systemctl daemon-reload
systemctl enable skyblock-price.service

printf '[4/4] Start service and verify local authentication\n'
systemctl restart skyblock-price.service
local_ready=false
for ((attempt=0; attempt<30; attempt++)); do
    if printf 'Authorization: Bearer %s\n' "$token" | curl --noproxy '*' --silent --fail \
        --connect-timeout 2 --max-time 3 --header @- "http://$probe_address/v1/status" >/dev/null; then
        local_ready=true
        break
    fi
    sleep 1
done
if [[ $local_ready != true ]]; then
    if [[ $has_previous == true ]]; then
        mv -f -- "$install_dir/skyblock-price-service.previous" "$install_dir/skyblock-price-service"
        systemctl restart skyblock-price.service || true
        printf 'Previous binary restored.\n' >&2
    fi
    fail 'Local service failed to start. Check journalctl -u skyblock-price -n 50.'
fi
printf '\nInstalled successfully.\nListen: %s\nAPI: http://SERVER_IP:%s\nAccess token: %s\n' "$bind_address" "$listen_port" "$token"
printf 'Keep this token private. Saved at: %s\n' "$env_file"
printf 'First synchronization runs in the background; /health/ready may temporarily return 503.\n'
printf 'Logs: sudo journalctl -u skyblock-price -f\n'
