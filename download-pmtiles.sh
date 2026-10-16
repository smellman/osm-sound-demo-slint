#!/usr/bin/env bash
# Copies the style's PMTiles archives from tile.openstreetmap.jp into pmtiles/,
# where the bundled style looks for them before falling back to the server.
#
#     ./download-pmtiles.sh                    # all three
#     ./download-pmtiles.sh takeshima hoppo    # just these
#
# planet.pmtiles is about 85 GB. An interrupted download resumes where it
# stopped, and an archive that is already up to date is skipped, so the script
# is safe to run again — which is also how to pick up a newer planet.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BASE_URL="${BASE_URL:-https://tile.openstreetmap.jp/static}"
DEST="$here/pmtiles"

if (($# > 0)); then
  names=("$@")
else
  names=(planet takeshima hoppo)
fi

mkdir -p "$DEST"

# One header from a HEAD response, without its trailing CR.
header() {
  awk -v name="$1" 'BEGIN { IGNORECASE = 1 } $1 == name ":" { sub(/^[^:]*: */, ""); sub(/\r$/, ""); print }'
}

for name in "${names[@]}"; do
  url="$BASE_URL/$name.pmtiles"
  file="$DEST/$name.pmtiles"
  part="$file.part"
  # The Last-Modified the partial file was started against. A resumed download
  # of a file that has since been replaced would splice two versions together.
  stamp="$part.last-modified"

  headers="$(curl --fail --silent --show-error --location --head "$url")"
  length="$(header content-length <<<"$headers" | tail -1)"
  modified="$(header last-modified <<<"$headers" | tail -1)"
  if [[ -z "$length" || -z "$modified" ]]; then
    echo "error: $url did not report its size and date" >&2
    exit 1
  fi

  if [[ -f "$file" && "$(stat -c %s "$file")" == "$length" ]] &&
     [[ "$(date -u -r "$file" +%s)" == "$(date -u -d "$modified" +%s)" ]]; then
    echo "==> $name.pmtiles is up to date ($modified)"
    continue
  fi

  if [[ -f "$part" && "$(cat "$stamp" 2>/dev/null)" != "$modified" ]]; then
    echo "==> $name.pmtiles changed on the server; starting over"
    rm -f "$part"
  fi
  printf '%s\n' "$modified" >"$stamp"

  echo "==> downloading $url ($length bytes, $modified)"
  curl --fail --location --continue-at - --remote-time --output "$part" "$url"

  actual="$(stat -c %s "$part")"
  if [[ "$actual" != "$length" ]]; then
    echo "error: $name.pmtiles is $actual bytes, expected $length" >&2
    exit 1
  fi
  # --remote-time is not applied to a resumed transfer, so set it here.
  touch -d "$modified" "$part"
  mv "$part" "$file"
  rm -f "$stamp"
  echo "==> $name.pmtiles done"
done
