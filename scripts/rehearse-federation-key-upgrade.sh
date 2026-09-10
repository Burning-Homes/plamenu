#!/usr/bin/env bash
# Production-shaped, disposable rehearsal for the federation key expand /
# backup / rollback / root-rotation / contract sequence.
set -euo pipefail

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
baseline_commit=${PLAMENU_BASELINE_COMMIT:-cc9642747bee9f5f192c41e7573694fd2709f92b}
pg_host=${PLAMENU_REHEARSAL_PGHOST:-127.0.0.1}
pg_port=${PLAMENU_REHEARSAL_PGPORT:-5433}
pg_user=${PLAMENU_REHEARSAL_PGUSER:-plamenu}
pg_password=${PLAMENU_REHEARSAL_PGPASSWORD:-plamenu}
run_id="${$}"
primary_db="plamenu_parity_${run_id}"
restore_db="${primary_db}_restore"
rollback_db="${primary_db}_rollback"
scratch=$(mktemp -d -t plamenu-key-rehearsal.XXXXXXXX)
baseline_tree="$scratch/baseline"
baseline_target="$repo_root/target/rehearsal-baseline"
baseline_bin="$baseline_target/debug/plamenu"
current_bin="$repo_root/target/debug/plamenu"
old_pid=""
new_pid=""
worktree_added=false
export PGPASSWORD="$pg_password"

case "$primary_db $restore_db $rollback_db" in
    *[!a-z0-9_\ ]*)
        echo "refusing unsafe rehearsal database name" >&2
        exit 1
        ;;
esac

cleanup() {
    if [[ -n "$old_pid" ]]; then kill "$old_pid" 2>/dev/null || true; fi
    if [[ -n "$new_pid" ]]; then kill "$new_pid" 2>/dev/null || true; fi
    dropdb --if-exists --force -h "$pg_host" -p "$pg_port" -U "$pg_user" "$primary_db" >/dev/null 2>&1 || true
    dropdb --if-exists --force -h "$pg_host" -p "$pg_port" -U "$pg_user" "$restore_db" >/dev/null 2>&1 || true
    dropdb --if-exists --force -h "$pg_host" -p "$pg_port" -U "$pg_user" "$rollback_db" >/dev/null 2>&1 || true
    if $worktree_added; then
        git -C "$repo_root" worktree remove --force "$baseline_tree" >/dev/null 2>&1 || true
    fi
    rm -rf -- "$scratch"
}
trap cleanup EXIT INT TERM

for tool in cargo createdb curl git openssl pg_dump pg_restore psql; do
    command -v "$tool" >/dev/null || {
        echo "missing required tool: $tool" >&2
        exit 1
    }
done

db_url() {
    printf 'postgres://%s:%s@%s:%s/%s' "$pg_user" "$pg_password" "$pg_host" "$pg_port" "$1"
}

sql() {
    local database=$1
    shift
    psql -X -v ON_ERROR_STOP=1 -qAt -h "$pg_host" -p "$pg_port" -U "$pg_user" -d "$database" "$@"
}

write_config() {
    local path=$1 database=$2 bind=$3 root=$4 version=$5 previous=${6:-}
    {
        printf 'domain = "upgrade-rehearsal.invalid"\n'
        printf 'database_url = "%s"\n' "$(db_url "$database")"
        printf 'bind = "%s"\n' "$bind"
        printf 'media_dir = "%s/media"\n' "$scratch"
        printf 'allow_private_fetch = true\n'
        printf 'encryption_secret = "%s"\n' "$root"
        if [[ -n "$version" ]]; then
            printf 'encryption_secret_version = %s\n' "$version"
        fi
        if [[ -n "$previous" ]]; then
            printf 'encryption_previous_secrets = ["%s"]\n' "$previous"
        fi
    } >"$path"
    chmod 600 "$path"
}

wait_ready() {
    local url=$1 pid=$2 log=$3
    for _ in $(seq 1 120); do
        if curl -fsS --max-time 1 "$url/ready" >/dev/null 2>&1; then return 0; fi
        if ! kill -0 "$pid" 2>/dev/null; then
            echo "server exited before readiness; log follows" >&2
            sed -n '1,160p' "$log" >&2
            return 1
        fi
        sleep 0.25
    done
    echo "server did not become ready; log follows" >&2
    sed -n '1,160p' "$log" >&2
    return 1
}

stop_server() {
    local pid=$1
    kill "$pid"
    wait "$pid" 2>/dev/null || true
}

echo "rehearsal baseline: $baseline_commit"
git -C "$repo_root" worktree add --detach "$baseline_tree" "$baseline_commit" >/dev/null
worktree_added=true
CARGO_TARGET_DIR="$baseline_target" cargo build --locked -q --manifest-path "$baseline_tree/Cargo.toml" -p plamenu --bin plamenu
cargo build --locked -q --manifest-path "$repo_root/Cargo.toml" -p plamenu --bin plamenu

root_v1=$(openssl rand -hex 32)
root_v2=$(openssl rand -hex 32)
printf '%s\n' "$root_v1" >"$scratch/root-v1.external"
printf '%s\n' "$root_v2" >"$scratch/root-v2.external"
chmod 600 "$scratch"/root-v*.external

createdb -T template0 -h "$pg_host" -p "$pg_port" -U "$pg_user" "$primary_db"
write_config "$scratch/old.toml" "$primary_db" "127.0.0.1:18420" "$root_v1" ""
write_config "$scratch/new-v1.toml" "$primary_db" "127.0.0.1:18421" "$root_v1" 1
write_config "$scratch/new-wrong.toml" "$primary_db" "127.0.0.1:18421" "$root_v2" 1
write_config "$scratch/new-v2.toml" "$primary_db" "127.0.0.1:18421" "$root_v2" 2 "1:$root_v1"

for name in legacy_a legacy_b legacy_c; do
    "$baseline_bin" --config "$scratch/old.toml" account add "$name" >/dev/null
done
baseline_accounts=$(sql "$primary_db" -c "SELECT count(*) FROM accounts WHERE domain IS NULL")
pg_dump -Fc -h "$pg_host" -p "$pg_port" -U "$pg_user" -d "$primary_db" -f "$scratch/pre-upgrade.dump"
baseline_backup_bytes=$(wc -c <"$scratch/pre-upgrade.dump")
baseline_key_storage_bytes=$(sql "$primary_db" -c "SELECT pg_total_relation_size('accounts') + pg_total_relation_size('instance_actor_keys')")

"$baseline_bin" --config "$scratch/old.toml" serve >"$scratch/old.log" 2>&1 &
old_pid=$!
wait_ready "http://127.0.0.1:18420" "$old_pid" "$scratch/old.log"
stop_server "$old_pid"
old_pid=""

expand_started=$(date +%s%N)
"$current_bin" --config "$scratch/new-v1.toml" serve >"$scratch/new-v1.log" 2>&1 &
new_pid=$!
wait_ready "http://127.0.0.1:18421" "$new_pid" "$scratch/new-v1.log"
expand_ms=$(( ( $(date +%s%N) - expand_started ) / 1000000 ))
private_rows=$(sql "$primary_db" -c "SELECT count(*) FROM actor_keys WHERE encrypted_private_key IS NOT NULL")
expanded_key_storage_bytes=$(sql "$primary_db" -c "SELECT pg_total_relation_size('actor_keys')")
legacy_rows=$(sql "$primary_db" -c "SELECT count(*) FROM accounts WHERE private_key IS NOT NULL OR ed25519_private_key IS NOT NULL")
[[ "$private_rows" -gt 0 && "$legacy_rows" -eq "$baseline_accounts" ]]
stop_server "$new_pid"
new_pid=""

if "$current_bin" --config "$scratch/new-wrong.toml" role list >"$scratch/wrong-root.log" 2>&1; then
    echo "wrong encryption root unexpectedly passed preflight" >&2
    exit 1
fi
if rg -n 'PRIVATE KEY|z3u2' "$scratch/wrong-root.log" >/dev/null; then
    echo "wrong-root error leaked private material" >&2
    exit 1
fi

pg_dump -Fc -h "$pg_host" -p "$pg_port" -U "$pg_user" -d "$primary_db" -f "$scratch/expanded.dump"
createdb -T template0 -h "$pg_host" -p "$pg_port" -U "$pg_user" "$restore_db"
pg_restore -h "$pg_host" -p "$pg_port" -U "$pg_user" -d "$restore_db" "$scratch/expanded.dump"
write_config "$scratch/restore.toml" "$restore_db" "127.0.0.1:18422" "$(<"$scratch/root-v1.external")" 1
"$current_bin" --config "$scratch/restore.toml" serve >"$scratch/restore.log" 2>&1 &
new_pid=$!
wait_ready "http://127.0.0.1:18422" "$new_pid" "$scratch/restore.log"
stop_server "$new_pid"
new_pid=""

createdb -T template0 -h "$pg_host" -p "$pg_port" -U "$pg_user" "$rollback_db"
pg_restore -h "$pg_host" -p "$pg_port" -U "$pg_user" -d "$rollback_db" "$scratch/pre-upgrade.dump"
write_config "$scratch/rollback.toml" "$rollback_db" "127.0.0.1:18423" "$root_v1" ""
"$baseline_bin" --config "$scratch/rollback.toml" serve >"$scratch/rollback.log" 2>&1 &
old_pid=$!
wait_ready "http://127.0.0.1:18423" "$old_pid" "$scratch/rollback.log"
stop_server "$old_pid"
old_pid=""

first_rewrapped=$(
    "$current_bin" --config "$scratch/new-v2.toml" federation keys rewrap --limit 1 |
        sed -n 's/^rewrapped \([0-9][0-9]*\).*/\1/p'
)
[[ "$first_rewrapped" == 1 ]]
old_versions=$(sql "$primary_db" -c "SELECT count(*) FROM actor_keys WHERE encrypted_private_key IS NOT NULL AND encryption_key_version = 1")
[[ "$old_versions" -gt 0 ]]
"$current_bin" --config "$scratch/new-v2.toml" federation keys rewrap >/dev/null
old_versions=$(sql "$primary_db" -c "SELECT count(*) FROM actor_keys WHERE encrypted_private_key IS NOT NULL AND encryption_key_version <> 2")
[[ "$old_versions" -eq 0 ]]

"$current_bin" --config "$scratch/new-v2.toml" federation keys contract >/dev/null
"$current_bin" --config "$scratch/new-v2.toml" federation keys audit >/dev/null
legacy_columns=$(sql "$primary_db" -c "SELECT count(*) FROM information_schema.columns WHERE table_schema = current_schema() AND ((table_name = 'accounts' AND column_name IN ('private_key','ed25519_private_key')) OR (table_name = 'instance_actor_keys' AND column_name IN ('private_key','ed25519_private_key')))")
[[ "$legacy_columns" -eq 0 ]]

pg_dump -Fp -h "$pg_host" -p "$pg_port" -U "$pg_user" -d "$primary_db" -f "$scratch/post-contract.sql"
if rg -n -- '-----BEGIN ([A-Z ]+ )?PRIVATE KEY-----|z3u2' "$scratch/post-contract.sql" >/dev/null; then
    echo "post-contract backup contains usable private signing material" >&2
    exit 1
fi

"$current_bin" --config "$scratch/new-v2.toml" serve >"$scratch/post-contract.log" 2>&1 &
new_pid=$!
wait_ready "http://127.0.0.1:18421" "$new_pid" "$scratch/post-contract.log"
stop_server "$new_pid"
new_pid=""

post_contract_bytes=$(wc -c <"$scratch/post-contract.sql")
echo "PASS baseline_accounts=$baseline_accounts private_rows=$private_rows expand_ms=$expand_ms baseline_backup_bytes=$baseline_backup_bytes baseline_key_storage_bytes=$baseline_key_storage_bytes expanded_key_storage_bytes=$expanded_key_storage_bytes post_contract_backup_bytes=$post_contract_bytes legacy_columns=$legacy_columns stale_root_versions=$old_versions"
