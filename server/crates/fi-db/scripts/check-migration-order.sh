#!/bin/sh
# Fails when a migration added on this branch does not sort after every migration on the base
# branch (CI step of SER-0.2; usage: crates/fi-db/scripts/check-migration-order.sh [base-ref]).
#
# sqlx applies pending files in version order, so a file merged with a timestamp below one that
# production already applied would run out of order, after newer schema it may not expect. Four
# initiatives add files to one folder: every new file must be newer than everything on main.
# Readiness (fi_db::migrate::check_applied) also refuses a database that misses any embedded file.
set -eu

base="${1:-origin/main}"
dir="crates/fi-db/migrations"
cd "$(git rev-parse --show-toplevel)"

if ! git rev-parse --verify --quiet "$base" >/dev/null; then
  echo "check-migration-order: base $base not found; nothing to compare (first commit?)"
  exit 0
fi

version() { basename "$1" | cut -d_ -f1; }

newest_on_base=$(git ls-tree --name-only "$base" "$dir/" | grep -E '/[0-9]{14}_[a-z0-9_]+\.sql$' | while read -r f; do version "$f"; done | sort | tail -1)
newest_on_base=${newest_on_base:-0}

status=0
for f in $(git diff --no-renames --name-only --diff-filter=A "$base"...HEAD -- "$dir"); do
  case "$f" in *.sql) ;; *) continue ;; esac
  v=$(version "$f")
  if ! echo "$v" | grep -Eq '^[0-9]{14}$'; then
    echo "check-migration-order: $f is not named <14-digit UTC timestamp>_<area>.sql"
    status=1
  elif [ "$v" -le "$newest_on_base" ]; then
    echo "check-migration-order: $f ($v) does not sort after $newest_on_base, the newest on $base: rename it with a current timestamp"
    status=1
  fi
done
for f in $(git diff --no-renames --name-only --diff-filter=MD "$base"...HEAD -- "$dir"); do
  echo "check-migration-order: $f exists on $base and was changed or deleted; once applied, migrations are forward-only (ADR 0004 §3)"
  status=1
done
exit $status
