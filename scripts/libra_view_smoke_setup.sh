#!/usr/bin/env bash
# Host-side setup for the opt-in Libra view smoke. The seed/register subcommands
# run inside interop-smoke so HTTP and git use the same loopback relay as cases.
set -euo pipefail

DB=mega2_hp25_views
REDIS_DB=1
case "${1:-}" in
    seed|register)
        BASE_URL=http://127.0.0.1:9000
        TOKEN="${MEGA2_IT_SEED_TOKEN:-}"
        [ -n "$TOKEN" ] || { echo 'hp25 setup: seed token is missing' >&2; exit 1; }
        export GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null

        register_view() {
            local spec="$1" name="$2" code id
            local body="$WORK/register.json"
            jq -n --arg filter_spec "$spec" --arg name "$name" \
                '{filter_spec: $filter_spec, name: $name}' > "$body.request"
            code=$(curl -sS --connect-timeout 5 --max-time 30 \
                -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
                --data @"$body.request" -o "$body" -w '%{http_code}' \
                "$BASE_URL/api/v1/views?wait=true")
            if [ "$code" != 200 ] || ! jq -e '.data.ready == true and .data.version == 1' "$body" >/dev/null; then
                echo "hp25 setup: registration failed (HTTP $code)" >&2
                return 1
            fi
            id=$(jq -r '.data.filter_id' "$body")
            [[ "$id" =~ ^[0-9a-f]{64}$ ]] || { echo 'hp25 setup: invalid filter ID' >&2; return 1; }
            printf '%s\n' "$id"
        }

        if [ "$1" = register ]; then
            WORK=$(mktemp -d /work/hp25-register.XXXXXX)
            trap 'rm -rf -- "$WORK"' EXIT
            case "${2:-}" in
                ok) register_view ':/project/hp25-view-smoke/ok' hp25-ok ;;
                l0) register_view ':/project/hp25-view-smoke/l0' hp25-l0 ;;
                *) echo 'hp25 setup: unknown registration' >&2; exit 2 ;;
            esac
            exit
        fi

        WORK=$(mktemp -d /work/hp25-setup.XXXXXX)
        trap 'rm -rf -- "$WORK"' EXIT
        code=$(curl -sS --connect-timeout 5 --max-time 30 \
            -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
            --data '{"path":"/project/hp25-view-smoke"}' -o "$WORK/provision.json" \
            -w '%{http_code}' "$BASE_URL/api/v1/path/provision")
        [ "$code" = 200 ] || { echo "hp25 setup: provision failed (HTTP $code)" >&2; exit 1; }

        git clone -q "$BASE_URL/project/hp25-view-smoke.git" "$WORK/repo" \
            >/dev/null 2>&1 || { echo 'hp25 setup: seed clone failed' >&2; exit 1; }
        parent=$(git -C "$WORK/repo" rev-parse HEAD)
        blob=$(printf 'hp25 fixed tree entry\n' | git -C "$WORK/repo" hash-object -w --stdin)
        ok=$(printf '100644 blob %s\tok.txt\n' "$blob" | git -C "$WORK/repo" mktree)
        unready=$(printf '100644 blob %s\tunready.txt\n' "$blob" | git -C "$WORK/repo" mktree)
        l0=$(printf '100664 blob %s\tbad.txt\n' "$blob" | git -C "$WORK/repo" mktree)
        root=$(printf '040000 tree %s\tl0\n040000 tree %s\tok\n040000 tree %s\tunready\n' \
            "$l0" "$ok" "$unready" | git -C "$WORK/repo" mktree)
        commit=$(printf 'HP-25 view smoke seed\n' | \
            GIT_AUTHOR_NAME='Mega2 Smoke' GIT_AUTHOR_EMAIL='mega2-smoke@example.invalid' \
            GIT_COMMITTER_NAME='Mega2 Smoke' GIT_COMMITTER_EMAIL='mega2-smoke@example.invalid' \
            git -C "$WORK/repo" commit-tree "$root" -p "$parent")
        git -C "$WORK/repo" -c "http.extraHeader=Authorization: Bearer $TOKEN" \
            push -q origin "$commit:refs/heads/main" >/dev/null 2>&1 \
            || { echo 'hp25 setup: seed push failed' >&2; exit 1; }
        register_view ':/project/hp25-view-smoke/unready' hp25-unready
        ;;
    reset|prepare)
        ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
        cd "$ROOT"
        COMPOSE=(docker compose -p mega2-trunk -f docker/docker-compose-storage-only.yml)
        if [ -z "$DB" ] || [ "$DB" = mega2 ] || [ -z "$REDIS_DB" ] || [ "$REDIS_DB" = 0 ]; then
            echo 'hp25 setup: unsafe isolation constants' >&2
            exit 1
        fi
        if [ "$1" = reset ]; then
            "${COMPOSE[@]}" exec -T postgres psql -U mega2 -d postgres -v ON_ERROR_STOP=1 \
                -c "DROP DATABASE IF EXISTS $DB WITH (FORCE)"
            "${COMPOSE[@]}" exec -T postgres psql -U mega2 -d postgres -v ON_ERROR_STOP=1 \
                -c "CREATE DATABASE $DB"
            "${COMPOSE[@]}" exec -T redis redis-cli -n "$REDIS_DB" FLUSHDB
            exit
        fi

        COMPOSE+=(-f docker/docker-compose-storage-only.views.yml)
        "${COMPOSE[@]}" exec -T mega2 mega2 --config /etc/mega2/config.toml service init --yes
        id=$("${COMPOSE[@]}" --profile interop exec -T interop-smoke \
            bash /repo/scripts/libra_view_smoke_setup.sh seed)
        [[ "$id" =~ ^[0-9a-f]{64}$ ]] || { echo 'hp25 setup: invalid seed filter ID' >&2; exit 1; }
        "${COMPOSE[@]}" stop mega2
        sql=$(awk '
            $0 == "```sql" {
                if (getline <= 0) exit 1
                if ($0 == "-- view-ops: recycle-filter") { matches++; copy = 1; print; next }
            }
            copy && $0 == "```" { copy = 0; next }
            copy { print }
            END { if (matches != 1 || copy) exit 1 }
        ' docs/deploy-trunk.md) || { echo 'hp25 setup: recycle SQL marker is not unique' >&2; exit 1; }
        sql=${sql//<filter_id>/$id}
        printf '%s\n' "$sql" | "${COMPOSE[@]}" exec -T postgres \
            psql -U mega2 -d "$DB" -v ON_ERROR_STOP=1 -f -
        "${COMPOSE[@]}" up -d --wait --no-deps --no-recreate mega2
        "${COMPOSE[@]}" --profile interop exec -T interop-smoke \
            bash /repo/scripts/libra_view_smoke_setup.sh register ok >/dev/null
        "${COMPOSE[@]}" --profile interop exec -T interop-smoke \
            bash /repo/scripts/libra_view_smoke_setup.sh register l0 >/dev/null
        ;;
    *)
        echo 'usage: libra_view_smoke_setup.sh <reset|prepare|seed|register ok|register l0>' >&2
        exit 2
        ;;
esac
