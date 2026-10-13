#!/usr/bin/env bash
# Build a demo library for the Linux build: films and a TV show with poster art, video, two audio
# languages and subtitle files beside each video — everything the local library and the player
# show, made from nothing but ffmpeg and the app's own fonts (no third-party media, nothing to
# license). Every title is invented.
#
#   design/demo-library/generate.sh [DIR]      (default: design/demo-library/library, the copy
#                                              kept in the repository)
#   PLXNATIVE_MEDIA_DIRS=design/demo-library/library dist/linux/plxnative/plxnative
#
# Takes a few minutes and about 40 MB at the defaults (VIDEO_SIZE=1280x720 CRF=26 gives a sharper
# ~90 MB copy). Re-running skips files that already exist; delete a file to remake it.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/../.." && pwd)
OUT=${1:-$ROOT/design/demo-library/library}
FONT="$ROOT/pkg/appfont.ttf"
BOLD="$ROOT/pkg/appfont-bold.ttf"
FILM_SECONDS=${FILM_SECONDS:-180}
EPISODE_SECONDS=${EPISODE_SECONDS:-90}
VIDEO_SIZE=${VIDEO_SIZE:-960x540}   # what the repository keeps; 1280x720 and CRF=26 look sharper
CRF=${CRF:-30}
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

command -v ffmpeg >/dev/null || { echo "ffmpeg is required" >&2; exit 1; }

ff() { ffmpeg -hide_banner -loglevel error -y "$@"; }

# A text file for drawtext, so titles need no filter escaping.
text() { local f="$TMP/t$RANDOM$RANDOM"; printf '%s' "$1" > "$f"; printf '%s' "$f"; }

# art OUT W H C0 C1 C2 TITLE LINE2 [TYPE]: a graded field, film grain, a vignette and the title.
# An empty TITLE gives a clean still (the frames the videos are made of).
art() {
    local out=$1 w=$2 h=$3 c0=$4 c1=$5 c2=$6 title=$7 line2=$8 type=${9:-radial}
    [ -f "$out" ] && return
    # The title fills at most ~85% of the width: shorter titles at w/9, longer ones smaller.
    local size=$((w / 9)) small=$((w / 26))
    local fit=$((w * 3 / (2 * (${#title} + 1))))
    [ ${#title} -gt 0 ] && [ $fit -lt $size ] && size=$fit
    local draw=""
    if [ -n "$title" ]; then
        draw=",drawtext=fontfile=$BOLD:textfile=$(text "$title"):fontsize=$size:fontcolor=white@0.95:x=(w-tw)/2:y=h*0.70:shadowcolor=black@0.5:shadowx=0:shadowy=4"
        draw+=",drawtext=fontfile=$FONT:textfile=$(text "$line2"):fontsize=$small:fontcolor=white@0.75:x=(w-tw)/2:y=h*0.70+${size}*1.35"
    fi
    ff -f lavfi -i "gradients=s=${w}x${h}:c0=$c0:c1=$c1:c2=$c2:nb_colors=3:type=$type:x0=$((w/3)):y0=$((h/4)):x1=$((w*4/5)):y1=$((h*9/10)):seed=7:d=1" \
        -vf "noise=alls=7:allf=t,vignette=PI/4.5${draw}" -frames:v 1 -q:v 3 "$out"
}

# audio OUT SECONDS F1 F2 F3: a quiet sustained chord, a stand-in for a soundtrack.
audio() {
    local out=$1 s=$2
    [ -f "$out" ] && return
    ff -f lavfi -i "aevalsrc=0.06*sin(2*PI*$3*t)+0.05*sin(2*PI*$4*t)+0.04*sin(2*PI*$5*t)|0.06*sin(2*PI*$3*t)+0.05*sin(2*PI*$4*t)+0.04*sin(2*PI*$5*t):s=48000:d=$s" \
        -af "afade=t=in:d=2,volume=0.8" "$out"
}

# subtitles OUT SECONDS LINE...: one cue every 12 seconds, cycling through the lines given.
subtitles() {
    local out=$1 s=$2; shift 2
    [ -f "$out" ] && return
    local n=1 t=4 lines=("$@") i=0
    : > "$out"
    while [ $t -lt $((s - 6)) ]; do
        printf '%d\n00:%02d:%02d,000 --> 00:%02d:%02d,500\n%s\n\n' $n $((t / 60)) $((t % 60)) $(((t + 4) / 60)) $(((t + 4) % 60)) "${lines[$i]}" >> "$out"
        n=$((n + 1)); t=$((t + 12)); i=$(((i + 1) % ${#lines[@]}))
    done
}

# video OUT STILL SECONDS TITLE_EN TITLE_ES: a slow push-in on a still, English AC3 and Spanish
# AAC audio, both tagged so the player's audio menu names them.
video() {
    local out=$1 still=$2 s=$3
    [ -f "$out" ] && return
    local frames=$((s * 24))
    audio "$TMP/en.wav" "$s" 220 277.18 329.63
    audio "$TMP/es.wav" "$s" 196 246.94 293.66
    ff -loop 1 -i "$still" -i "$TMP/en.wav" -i "$TMP/es.wav" \
        -filter_complex "[0:v]scale=2560:-2,zoompan=z='1+0.12*on/$frames':x='iw/2-(iw/zoom/2)':y='ih/2-(ih/zoom/2)':d=$frames:s=$VIDEO_SIZE:fps=24,format=yuv420p[v]" \
        -map "[v]" -map 1:a -map 2:a -t "$s" \
        -c:v libx264 -preset veryfast -crf "$CRF" -tune stillimage \
        -c:a:0 ac3 -b:a:0 192k -c:a:1 aac -b:a:1 128k \
        -metadata:s:a:0 language=eng -metadata:s:a:0 title="English" \
        -metadata:s:a:1 language=spa -metadata:s:a:1 title="Español" \
        "$out"
    rm -f "$TMP/en.wav" "$TMP/es.wav"
}

EN=("Did you see that?" "We should have left an hour ago." "Keep the light on the water." "It's further than it looks." "Nobody comes out here anymore." "Listen. Do you hear it?")
ES=("¿Viste eso?" "Deberíamos habernos ido hace una hora." "Mantén la luz sobre el agua." "Está más lejos de lo que parece." "Ya nadie viene por aquí." "Escucha. ¿Lo oyes?")

# film "Title" year c0 c1 c2 "tagline" [type]
film() {
    local title=$1 year=$2 c0=$3 c1=$4 c2=$5 tagline=$6 type=${7:-radial}
    local dir="$OUT/Films/$title ($year)" name="$title ($year)"
    mkdir -p "$dir"
    echo "film: $name"
    art "$dir/poster.jpg" 1000 1500 "$c0" "$c1" "$c2" "$title" "$tagline" "$type"
    art "$TMP/still.jpg" 2560 1440 "$c0" "$c1" "$c2" "" "" "$type"
    video "$dir/$name.mkv" "$TMP/still.jpg" "$FILM_SECONDS"
    rm -f "$TMP/still.jpg"
    subtitles "$dir/$name.en.srt" "$FILM_SECONDS" "${EN[@]}"
    subtitles "$dir/$name.es.srt" "$FILM_SECONDS" "${ES[@]}"
}

# show "Title" c0 c1 c2 "tagline" seasons... where each season is "N:Ep title|Ep title|..."
show() {
    local title=$1 c0=$2 c1=$3 c2=$4 tagline=$5; shift 5
    mkdir -p "$OUT/TV Shows/$title"
    echo "show: $title"
    art "$OUT/TV Shows/$title/poster.jpg" 1000 1500 "$c0" "$c1" "$c2" "$title" "$tagline" circular
    for season in "$@"; do
        local n=${season%%:*} eps=${season#*:} e=0
        local dir="$OUT/TV Shows/$title/Season $n"
        mkdir -p "$dir"
        art "$dir/folder.jpg" 1000 1500 "$c1" "$c0" "$c2" "Season $n" "$title" circular
        IFS='|' read -ra names <<< "$eps"
        for ep in "${names[@]}"; do
            e=$((e + 1))
            local base
            base=$(printf '%s - S%02dE%02d - %s' "$title" "$n" "$e" "$ep")
            art "$dir/$base.jpg" 1000 1500 "$c2" "$c1" "$c0" "$ep" "$(printf 'Season %d · Episode %d' "$n" "$e")" linear
            art "$TMP/still.jpg" 2560 1440 "$c2" "$c0" "$c1" "" "" linear
            video "$dir/$base.mkv" "$TMP/still.jpg" "$EPISODE_SECONDS"
            rm -f "$TMP/still.jpg"
            subtitles "$dir/$base.en.srt" "$EPISODE_SECONDS" "${EN[@]}"
        done
    done
}

mkdir -p "$OUT/Films" "$OUT/TV Shows"
art "$OUT/Films/folder.jpg" 1000 1500 0x1d2b53 0x7e2553 0xff77a8 "Films" "Six features" spiral
art "$OUT/TV Shows/folder.jpg" 1000 1500 0x0b3d2e 0x1f8a70 0xbedb39 "TV Shows" "Two series" spiral

film "Aurora"          2023 0x0b1d3a 0x1c7c7d 0x9b5de5 "The sky remembers"
film "Deep Field"      2021 0x05070f 0x1b2a49 0xe0a526 "Look long enough"
film "The Long Light"  2019 0x2b0a0a 0xc0392b 0xf5b041 "One summer. No nights."
film "Tidewater"       2024 0x032b2f 0x0f6e74 0x6fd3c7 "What the sea gives back"
film "Signal and Noise" 2022 0x150016 0x8e1f74 0xff4f9a "Somebody is listening" linear
film "Paper Moons"     2020 0x1a1440 0x5b4b9a 0xf3e9d2 "A love story, folded twice"

show "Orbit Station" 0x020617 0x1e3a8a 0x38bdf8 "Two hundred miles up" \
    "1:Arrival|Pressure|Night Shift|The Long Way Down" \
    "2:Drift|Signal"
show "Coastal Kitchen" 0x3b1d0a 0xd97706 0xfde68a "Cooking where the land ends" \
    "1:Salt|Smoke|Harvest"

echo "demo library: $OUT"
