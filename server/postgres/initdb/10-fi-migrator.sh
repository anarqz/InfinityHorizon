#!/bin/sh
# Self-hosted server (compose.yml): run once by the postgres image when its data directory is
# empty (docker-entrypoint-initdb.d).
#
#   fi_migrator  LOGIN, CREATEROLE, CREATEDB, never SUPERUSER (as a managed database's user): it
#                owns forza_infinity and creates the group roles, so `fi-platform migrate` can grant
#                them to the runtime logins it creates (fi_app, fi_jobs).
#
# The password comes from the keys volume (`fi-platform keys init`), copied by the compose's
# entrypoint to /run/fi-keys (owned by postgres, 0400): this script runs as postgres, which cannot
# read the volume's 0600 files owned by the platform image's nonroot user.
set -eu

password_file=/run/fi-keys/FI_MIGRATOR_PASSWORD
[ -s "$password_file" ] || { echo "10-fi-migrator.sh: $password_file is missing" >&2; exit 1; }
FI_MIGRATOR_PASSWORD=$(cat "$password_file")

psql --no-psqlrc --set=ON_ERROR_STOP=1 \
  --username "$POSTGRES_USER" --dbname "$POSTGRES_DB" \
  --set=migrator_password="$FI_MIGRATOR_PASSWORD" <<'SQL'
SELECT format(
  'CREATE ROLE fi_migrator WITH LOGIN CREATEROLE CREATEDB NOSUPERUSER NOBYPASSRLS NOREPLICATION PASSWORD %L',
  :'migrator_password')
WHERE NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'fi_migrator')
\gexec

SELECT 'CREATE DATABASE forza_infinity OWNER fi_migrator'
WHERE NOT EXISTS (SELECT FROM pg_database WHERE datname = 'forza_infinity')
\gexec
SQL
