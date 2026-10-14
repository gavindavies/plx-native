#!/usr/bin/env python3
"""Build PiTV's demo library from the openly licensed catalog beside this script.

    design/demo-library/build.py [DIR]          (default: design/demo-library/library)
    PITV_MEDIA_DIRS=design/demo-library/library dist/linux/pitv/pitv

`catalog.json` describes 27 films and 2 shows: Blender Studio's open films, public-domain classics
and ESA/Hubble's Hubblecast. `assets.json` pins every picture they use (URL, sha256, byte size,
pixel size, licence, author, attribution). This script

  * reads each asset from `sources/` beside this script (committed, so nothing is downloaded), or
    fetches one that is missing from its pinned URL, refusing a file whose hash or size differs;
  * writes the folder layout PiTV reads (`rust-modules/data/src/local.rs`):
        Films/poster.jpg, TV Shows/poster.jpg   (mosaics of the posters inside)
        Films/<Title> (<Year>)/poster.jpg  and  <Title> (<Year>).mkv
        TV Shows/<Show>/poster.jpg
        TV Shows/<Show>/Season 01/<Show> - S01E01 - <Episode>.mkv  and  .jpg beside it
    with posters and stills cut per the catalog's recipes;
  * gives every title a video: the real film where the catalog pins one (Sintel, 282 MB; skip it
    with --no-films), else a short H.264/AAC clip that pans across the title's own key art, with
    an English subtitle file holding its synopsis;
  * writes CREDITS.md (licence and attribution for every asset used) at the library's root.

Needs python3, ffmpeg/ffprobe and (only to fetch a missing source) curl. Re-running skips what already exists; delete a
file to remake it. Takes a few minutes of encoding.
"""
import argparse
import hashlib
import json
import os
import pathlib
import re
import shutil
import subprocess
import sys
import tempfile
import time
import urllib.request

HERE = pathlib.Path(__file__).resolve().parent
UA = "PiTV/1.0 (https://github.com/gavindavies/plx-native)"
FONT = HERE.parent.parent / "pkg" / "appfont.ttf"
POSTER = (1000, 1500)
POLITE_HOSTS = ("upload.wikimedia.org",)
POLITE_PAUSE = 6.0
RETRY_AFTER_CAP = 600
ATTEMPTS = 6
STILL = (1280, 720)


def cache_dir():
    """Where the sources live: `sources/` beside this script, committed (Sintel as 95 MiB parts),
    so a checkout builds the library without a download. `PITV_DEMO_CACHE` points elsewhere."""
    env = os.environ.get("PITV_DEMO_CACHE")
    return pathlib.Path(env) if env else HERE


def sha256(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for block in iter(lambda: f.read(1 << 20), b""):
            h.update(block)
    return h.hexdigest()


def stored(asset):
    """What is actually stored for an asset: its `file` pin where the original could not be fetched
    (Wikimedia refuses bulk original-size downloads; those are its 1280-px thumbnails), else the
    original itself. Licence, author and source page are the original's either way."""
    return asset.get("file", asset)


def source_path(aid, asset):
    ext = pathlib.PurePosixPath(urllib.request.urlparse(stored(asset)["url"]).path).suffix.lower() or ".bin"
    return cache_dir() / "sources" / f"{aid}{ext}"


def fetch(aid, asset):
    """The verified local copy of one asset, downloading it if it is missing or wrong."""
    dst = source_path(aid, asset)
    asset = stored(asset)
    # A file over GitHub's 100 MiB limit (the film) is committed as numbered byte-for-byte parts.
    parts = sorted(dst.parent.glob(dst.name + ".part-*"))
    if parts and not dst.exists():
        with open(dst, "wb") as out:
            for part in parts:
                with open(part, "rb") as f:
                    shutil.copyfileobj(f, out)
    if dst.exists() and dst.stat().st_size == asset["bytes"] and sha256(dst) == asset["sha256"]:
        return dst
    dst.parent.mkdir(parents=True, exist_ok=True)
    print(f"fetch {aid} ({asset['bytes']:,} bytes)", flush=True)
    if urllib.request.urlparse(asset["url"]).hostname in POLITE_HOSTS:
        time.sleep(POLITE_PAUSE)  # Wikimedia answers a burst with 429
    fd, tmp = tempfile.mkstemp(dir=dst.parent)
    os.close(fd)
    try:  # the partial download never survives: a failed request, a bad hash, a ^C
        # curl rather than urllib: Wikimedia refuses urllib's requests (403/429) whatever the UA.
        for attempt in range(1, ATTEMPTS + 1):
            r = subprocess.run(["curl", "-sSL", "-A", UA, "--max-time", "1800", "-o", tmp,
                                "-D", "-", "-w", "%{http_code}", asset["url"]],
                               capture_output=True, text=True)
            code = r.stdout[-3:]
            if r.returncode == 0 and code == "200":
                break
            if attempt == ATTEMPTS or (code not in ("429", "000") and not code.startswith("5")):
                sys.exit(f"{aid}: HTTP {code} {r.stderr.strip()} ({asset['url']})")
            m = re.findall(r"(?im)^retry-after:\s*(\d+)", r.stdout)
            wait = min(int(m[-1]) if m else 5 * 2 ** attempt, RETRY_AFTER_CAP)
            print(f"  HTTP {code}; retry {attempt}/{ATTEMPTS - 1} in {wait}s", flush=True)
            time.sleep(wait)
        got = sha256(tmp)
        if got != asset["sha256"] or os.path.getsize(tmp) != asset["bytes"]:
            sys.exit(f"{aid}: sha256 {got} does not match the pin ({asset['url']})")
        os.replace(tmp, dst)
    finally:
        if os.path.exists(tmp):
            os.unlink(tmp)
    return dst


def ffmpeg(args):
    subprocess.run(["ffmpeg", "-hide_banner", "-loglevel", "error", "-y"] + args, check=True)


def cover(w, h, anchor):
    """Scale to COVER w×h, then crop at the centre, an edge, or `upper` (a portrait's face line)."""
    x = {"right": "iw-ow", "left": "0"}.get(anchor, "(iw-ow)/2")
    y = "(ih-oh)*3/10" if anchor == "upper" else "(ih-oh)/2"
    return f"scale={w}:{h}:force_original_aspect_ratio=increase:flags=lanczos,crop={w}:{h}:{x}:{y}"


def derive_image(src, dst, size, recipe):
    """One image cut per its catalog recipe: `mode: cover` (fill and crop at `anchor`) or
    `mode: extend` (a wide banner fitted to the height over a blurred stretch of itself)."""
    if dst.exists():
        return
    w, h = size
    anchor = recipe.get("anchor") or {"focus-right": "right", "upper": "upper"}.get(recipe.get("crop", ""), "center")
    if recipe.get("mode") == "extend":
        x = "W-w" if anchor == "right" else "(W-w)/2"
        graph = (f"[0:v]format=rgb24,split[a][b];[a]{cover(w, h, 'center')},boxblur=40:3,eq=brightness=-0.10[bg];"
                 f"[b]scale=-2:{h}:flags=lanczos,crop='min(iw,{w})':{h}:'max(iw-{w},0)':0[fg];"
                 f"[bg][fg]overlay=x={x}:y=0,format=yuvj444p[o]")
    else:
        graph = f"[0:v]format=rgb24,{cover(w, h, anchor)},format=yuvj444p[o]"
    tmp = dst.with_name(dst.stem + ".part.jpg")
    ffmpeg(["-i", str(src), "-filter_complex", graph, "-map", "[o]", "-frames:v", "1", "-q:v", "3", str(tmp)])
    os.replace(tmp, dst)


def sentences(text):
    return [s.strip() for s in re.split(r"(?<=[.!?])\s+", text or "") if s.strip()]


def srt(lines, seconds):
    """One cue per line, evenly through the clip after a two-second lead-in."""
    if not lines:
        return ""
    step = max(3.0, (seconds - 2) / len(lines))
    out = []
    for i, line in enumerate(lines):
        a, b = 2 + i * step, min(seconds, 2 + (i + 1) * step - 0.3)
        if a >= seconds:
            break
        ts = lambda t: f"{int(t // 3600):02}:{int(t % 3600 // 60):02}:{int(t % 60):02},{int(t * 1000 % 1000):03}"
        out.append(f"{len(out) + 1}\n{ts(a)} --> {ts(b)}\n{line}\n")
    return "\n".join(out)


def clip(still, dst, seconds, title, lines):
    """A `seconds`-long H.264/AAC MKV that pans across `still` (already 1280×720), with the title
    over its first seconds and an English subtitle file beside it."""
    if not dst.exists():
        fps = 24
        frames = seconds * fps
        # A slow push-in across the art, the title fading out over the first four seconds.
        # A temp file with a plain name: the path sits inside the filter graph, where a comma or
        # colon from a title ("The future of Hubble, part 1") would end the argument.
        fd, name = tempfile.mkstemp(suffix=".txt")
        os.close(fd)
        title_file = pathlib.Path(name)
        title_file.write_text(title)
        draw = (f",drawtext=fontfile={FONT}:textfile={title_file}:fontsize=64:fontcolor=white:"
                f"x=(w-tw)/2:y=h*0.78:shadowcolor=black@0.6:shadowy=3:"
                f"alpha='if(lt(t,3),1,max(0,1-(t-3)))'") if FONT.exists() else ""
        video = (f"[0:v]scale=2560:1440,zoompan=z='1+0.12*on/{frames}':x='iw/2-(iw/zoom/2)':"
                 f"y='ih/2-(ih/zoom/2)':d={frames}:s=1280x720:fps={fps},format=yuv420p{draw}[v]")
        audio = (f"aevalsrc='0.05*sin(2*PI*220*t)+0.04*sin(2*PI*277.2*t)+0.03*sin(2*PI*329.6*t)'"
                 f":s=48000:c=stereo:d={seconds},afade=t=in:d=2,afade=t=out:st={seconds - 2}:d=2")
        tmp = dst.with_name(dst.stem + ".part.mkv")
        try:
            ffmpeg(["-loop", "1", "-i", str(still), "-f", "lavfi", "-i", audio,
                    "-filter_complex", video, "-map", "[v]", "-map", "1:a", "-t", str(seconds),
                    "-c:v", "libx264", "-preset", "veryfast", "-crf", "24",
                    "-c:a", "aac", "-b:a", "128k", "-metadata:s:a:0", "language=eng", str(tmp)])
            os.replace(tmp, dst)
        finally:
            title_file.unlink(missing_ok=True)
            tmp.unlink(missing_ok=True)
    sub = dst.with_suffix(".en.srt")
    if not sub.exists() and lines:
        sub.write_text(srt(lines, seconds))


def safe(name):
    return re.sub(r'[/\\:*?"<>|]', "", name).strip()


class Builder:
    def __init__(self, out, films, film_seconds, episode_seconds):
        self.assets = json.loads((HERE / "assets.json").read_text())["assets"]
        self.catalog = json.loads((HERE / "catalog.json").read_text())
        self.out, self.films = out, films
        self.film_seconds, self.episode_seconds = film_seconds, episode_seconds
        self.used = set()

    def src(self, aid):
        self.used.add(aid)
        return fetch(aid, self.assets[aid])

    def image(self, recipe, dst, size):
        if not dst.exists():
            derive_image(self.src(recipe["asset"]), dst, size, recipe)
        else:
            self.used.add(recipe["asset"])

    def movie(self, m):
        name = safe(f"{m['title']} ({m['year']})")
        d = self.out / "Films" / name
        d.mkdir(parents=True, exist_ok=True)
        self.image(m["poster"], d / "poster.jpg", POSTER)
        media = m.get("media")
        if media and self.films:
            asset = self.assets[media["asset"] if isinstance(media, dict) else media]
            aid = media["asset"] if isinstance(media, dict) else media
            ext = source_path(aid, asset).suffix
            film = d / f"{name}{ext}"
            if not film.exists():
                shutil.copyfile(self.src(aid), film)
            else:
                self.used.add(aid)
            return
        art = self.out / ".work" / f"{m['id']}-art.jpg"
        art.parent.mkdir(parents=True, exist_ok=True)
        self.image(m["art"], art, STILL)
        lines = ([m["tagline"]] if m.get("tagline") else []) + sentences(m.get("summary"))
        clip(art, d / f"{name}.mkv", self.film_seconds, m["title"], lines)

    def show(self, s):
        root = self.out / "TV Shows" / safe(s["title"])
        root.mkdir(parents=True, exist_ok=True)
        self.image(s["poster"], root / "poster.jpg", POSTER)
        for season in s["seasons"]:
            sd = root / f"Season {season['index']:02}"
            sd.mkdir(exist_ok=True)
            if not (sd / "poster.jpg").exists():
                shutil.copyfile(root / "poster.jpg", sd / "poster.jpg")
            for n, e in enumerate(season["episodes"], 1):
                stem = safe(f"{s['title']} - S{season['index']:02}E{n:02} - {e['title']}")
                thumb = sd / f"{stem}.jpg"
                self.image(e.get("thumb") or s["art"], thumb, STILL)
                clip(thumb, sd / f"{stem}.mkv", self.episode_seconds, e["title"], sentences(e.get("summary")))

    def mosaic(self, folder):
        """A folder's own poster: its first four titles' posters, two by two (fewer than four: the
        first one)."""
        dst = folder / "poster.jpg"
        posters = sorted(folder.glob("*/poster.jpg"))[:4]
        if dst.exists() or not posters:
            return
        if len(posters) < 4:  # too few to tile: the first title's poster
            shutil.copyfile(posters[0], dst)
            return
        w, h = POSTER[0] // 2, POSTER[1] // 2
        scaled = "".join(f"[{i}:v]scale={w}:{h}[p{i}];" for i in range(4))
        graph = scaled + "[p0][p1][p2][p3]xstack=inputs=4:layout=0_0|w0_0|0_h0|w0_h0,format=yuvj444p[o]"
        ffmpeg([a for p in posters for a in ("-i", str(p))] + ["-filter_complex", graph, "-map", "[o]",
                                                                 "-frames:v", "1", "-q:v", "3", str(dst)])

    def credits(self):
        rows = ["# Demo library credits", "",
                "Every picture in this library is openly licensed or in the public domain. The videos "
                "are made from those pictures by `design/demo-library/build.py`, except where a "
                "title's own film is listed below.", "",
                "| Asset | Licence | Author | Attribution | Source |", "|---|---|---|---|---|"]
        cell = lambda v: str(v).replace("|", "\\|")
        for aid in sorted(self.used):
            a = self.assets[aid]
            rows.append("| " + " | ".join(cell(v) for v in (aid, a["licence"], a["author"], a.get("attribution", ""),
                                                            a.get("source_page", a["url"]))) + " |")
        (self.out / "CREDITS.md").write_text("\n".join(rows) + "\n")

    def build(self):
        for m in self.catalog["movies"]:
            print(f"film: {m['title']}", flush=True)
            self.movie(m)
        for s in self.catalog["shows"]:
            print(f"show: {s['title']}", flush=True)
            self.show(s)
        self.mosaic(self.out / "Films")
        self.mosaic(self.out / "TV Shows")
        shutil.rmtree(self.out / ".work", ignore_errors=True)
        self.credits()
        print(f"demo library: {self.out}")


def main():
    p = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    p.add_argument("out", nargs="?", default=str(HERE / "library"))
    p.add_argument("--no-films", action="store_true", help="skip downloading the real films (Sintel, 282 MB)")
    p.add_argument("--film-seconds", type=int, default=60)
    p.add_argument("--episode-seconds", type=int, default=30)
    args = p.parse_args()
    for tool in ("ffmpeg", "ffprobe", "curl"):
        if not shutil.which(tool):
            sys.exit(f"{tool} is required")
    Builder(pathlib.Path(args.out), not args.no_films, args.film_seconds, args.episode_seconds).build()


if __name__ == "__main__":
    main()
