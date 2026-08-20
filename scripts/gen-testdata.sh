#!/usr/bin/env bash
# Generate the committed fixture library in `testdata-fixture/`.
#
# Why synthetic: the smoke suite needs a library that every clone has, but real music can't be
# committed (see NOTICE — `testdata/` holds a real, copyrighted library and stays git-ignored).
# Tagged sine waves decode in Chromium exactly like music does, so the playback hot path is
# exercised for real, while the tags are chosen to cover the cases a scanner gets wrong:
# unicode, a compilation whose album artist differs from its track artists, a multi-disc set,
# an untagged file, and a track with no album.
#
# The generated files ARE committed, so neither CI nor a contributor needs ffmpeg. Re-run this
# only to change the fixture — and re-check the counts in tests/ui/v2-flows.mjs if you do.
#
#   scripts/gen-testdata.sh
set -euo pipefail
cd "$(dirname "$0")/.."

OUT="testdata-fixture"

command -v ffmpeg >/dev/null || { echo "gen-testdata: needs ffmpeg" >&2; exit 1; }

rm -rf "$OUT"

# Tracks must OUTLIVE the smoke suite's interactions with them, which is the whole reason for
# this duration. Two checks depend on it: "seek jumps the playback position" drags to 50% and
# needs that to land well past where playback already is, and "eq does not disturb now-playing"
# fails if the track ends and auto-advances while the EQ section is being driven. Anything
# under ~20s fails both. Don't shorten this to save bytes.
DURATION=60

# Everything is a mono sine, encoded as small as it can be while still decoding in Chromium:
# 22.05 kHz / 8 kbps MP3 is ~61 KB a minute, 16 kHz FLAC ~296 KB. (Counter-intuitively, a
# quieter sine makes FLAC *bigger* — the volume filter's float conversion adds noise that
# doesn't compress — so these run at full amplitude.)
track() {
  local path="$1" freq="$2" codec="$3"; shift 3
  mkdir -p "$(dirname "$path")"
  local args=()
  while [ $# -gt 0 ]; do args+=(-metadata "$1"); shift; done

  local rate codec_args
  if [ "$codec" = mp3 ]; then
    rate=22050; codec_args="-codec:a libmp3lame -b:a 8k"
  else
    rate=16000; codec_args="-codec:a flac -compression_level 12"
  fi

  ffmpeg -hide_banner -loglevel error -y \
    -f lavfi -i "sine=frequency=${freq}:duration=${DURATION}:sample_rate=${rate}" \
    -ac 1 "${args[@]}" $codec_args "$path"
}

# --- A plain album: the ordinary case everything else is measured against. ------------------
for i in 1 2 3 4; do
  track "$OUT/The Meridian/Neon Hours/$(printf '%02d' "$i") Track.mp3" $((220 + i * 55)) mp3 \
    "title=Neon Hours Part $i" "artist=The Meridian" "album=Neon Hours" \
    "album_artist=The Meridian" "track=$i/4" "date=2021" "genre=Electronic"
done

# --- Unicode throughout, and FLAC rather than MP3, so neither path is assumed. --------------
i=0
for t in "Þrír Vetur" "Snæfell" "Ölduró"; do
  i=$((i + 1))
  track "$OUT/Sigrún Ólafsdóttir/Þrír Vetur/$(printf '%02d' "$i") ${t}.flac" $((330 + i * 40)) flac \
    "title=$t" "artist=Sigrún Ólafsdóttir" "album=Þrír Vetur" \
    "album_artist=Sigrún Ólafsdóttir" "track=$i/3" "date=2019" "genre=Ambient"
done

# --- A compilation: album artist deliberately differs from every track artist, which is what
#     splits an album into one-per-artist when a scanner groups on the wrong field. ----------
i=0
for pair in "Kestrel Lane|Low Tide" "Aster Vaux|Paper Harbour" "Nine Volt Sun|Transmit"; do
  i=$((i + 1))
  artist="${pair%%|*}"; title="${pair##*|}"
  track "$OUT/Various Artists/Collected Works/$(printf '%02d' "$i") ${title}.mp3" $((196 + i * 70)) mp3 \
    "title=$title" "artist=$artist" "album=Collected Works" \
    "album_artist=Various Artists" "track=$i/3" "date=2018" "genre=Compilation"
done

# --- Multi-disc: two discs whose track numbers both start at 1. ----------------------------
for d in 1 2; do
  for i in 1 2; do
    track "$OUT/Harbourmaster/Long Player/Disc $d/$(printf '%02d' "$i") Movement.mp3" $((140 + d * 90 + i * 30)) mp3 \
      "title=Movement $d.$i" "artist=Harbourmaster" "album=Long Player" \
      "album_artist=Harbourmaster" "track=$i/2" "disc=$d/2" "date=2023" "genre=Post-Rock"
  done
done

# --- No tags at all: the scanner must fall back to the filename. ---------------------------
track "$OUT/Loose Files/an untagged recording.mp3" 512 mp3

# --- Tagged, but with no album: the "singles" path. -----------------------------------------
track "$OUT/Loose Files/Vesper - Half Light.mp3" 610 mp3 \
  "title=Half Light" "artist=Vesper" "date=2024" "genre=Folk"

count=$(find "$OUT" -type f \( -name '*.mp3' -o -name '*.flac' \) | wc -l)
size=$(du -sh "$OUT" | cut -f1)
echo "gen-testdata: wrote $count tracks to $OUT/ ($size)"
