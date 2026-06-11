"""Seed an ADS store with thousands of synthetic assets (schema v8).

Publish tier (HTTP): drives a running `ads serve` instance. Objects are
uploaded first, then version records are imported with client-side canonical
manifest hashes — about 60% as WIP promotions (`promoted_from` + staging
source paths), the rest as direct `add --source` registrations — then
thumbnails and current pins are applied.

WIP tier (CLI): WIP streams are local-only by design and have no HTTP API, so
they are seeded by driving the real `ads wip add` / `publish promote`
workflow against the store directly. Stop `ads serve` first (RocksDB allows a
single writer process).

Usage:
    python scripts/seed_assets.py --server http://127.0.0.1:8799 \
        --token demo-token --profile main --target 3200
    # then, with ads serve stopped:
    python scripts/seed_assets.py --wip-store target/webapp_tmp/store \
        --ads target/debug/ads.exe --wip-departments 40
"""

from __future__ import annotations

import argparse
import hashlib
import io
import json
import random
import struct
import sys
import time
import zlib
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "python" / "src"))

from ads import AdsHttpClient  # noqa: E402

ADJECTIVES = [
    "rusty", "broken", "ancient", "polished", "burnt", "mossy", "gilded",
    "cracked", "heavy", "hollow", "iron", "stone", "oak", "brass", "ashen",
    "pale", "crimson", "azure", "drift", "ember", "frost", "dusty", "carved",
    "woven", "ruined", "grand", "small", "twin", "lone", "north",
]

NOUNS = [
    "crate", "barrel", "lantern", "sword", "shield", "pillar", "arch",
    "fence", "cart", "wheel", "anvil", "altar", "statue", "fountain",
    "bridge", "tower", "gate", "well", "bench", "chest", "banner", "kettle",
    "ladder", "beam", "tile", "rooftop", "chimney", "door", "window", "stair",
    "boulder", "stump", "fern", "pine", "willow", "reed", "vine", "root",
]

CREATURES = [
    "hero", "guard", "merchant", "smith", "rider", "scout", "witch",
    "golem", "wolf", "raven", "stag", "boar", "drake", "sprite", "warden",
]

VEHICLES = ["wagon", "skiff", "glider", "carriage", "sled", "barge", "cycle"]

# (category, kind, weight) — kind picks the name pool and department profile.
CATEGORIES = [
    ("char", "creature", 9),
    ("char/crowd", "creature", 4),
    ("char/creature", "creature", 5),
    ("prop", "prop", 16),
    ("prop/weapon", "prop", 6),
    ("prop/furniture", "prop", 7),
    ("prop/kitchen", "prop", 4),
    ("env", "env", 6),
    ("env/city", "env", 8),
    ("env/forest", "env", 7),
    ("env/harbor", "env", 5),
    ("env/interior", "env", 6),
    ("veh", "vehicle", 4),
    ("fx", "prop", 3),
    ("mat", "prop", 4),
]

DEPARTMENTS = {
    "creature": [("model", 10), ("lookdev", 7), ("rig", 6), ("anim", 5), ("texture", 6), ("fx", 2)],
    "prop": [("model", 10), ("lookdev", 6), ("texture", 5), ("rig", 1)],
    "env": [("model", 10), ("lookdev", 5), ("layout", 4), ("texture", 4)],
    "vehicle": [("model", 10), ("lookdev", 5), ("rig", 4), ("texture", 3), ("anim", 2)],
}

FILE_KINDS = [
    ("{code}.usd", 0),
    ("geo/{code}_geo.usd", 0),
    ("geo/{code}_proxy.usd", 0),
    ("mtl/{code}_mtl.usd", 0),
    ("maps/{code}_diffuse.1001.tx", 1),
    ("maps/{code}_rough.1001.tx", 1),
    ("maps/{code}_normal.1001.tx", 1),
    ("notes/readme.txt", 2),
]


def weighted_choice(rng, pairs):
    total = sum(weight for _, weight in pairs)
    pick = rng.uniform(0, total)
    acc = 0.0
    for value, weight in pairs:
        acc += weight
        if pick <= acc:
            return value
    return pairs[-1][0]


def make_png(path, width, height, top, bottom, stripe, step):
    rows = []
    for y in range(height):
        t = y / (height - 1)
        r = int(top[0] + (bottom[0] - top[0]) * t)
        g = int(top[1] + (bottom[1] - top[1]) * t)
        b = int(top[2] + (bottom[2] - top[2]) * t)
        row = bytearray()
        for x in range(width):
            if (x + y) % step < 5:
                row += bytes(stripe)
            else:
                row += bytes((r, g, b))
        rows.append(b"\x00" + bytes(row))
    raw = b"".join(rows)

    def chunk(tag, data):
        out = struct.pack(">I", len(data)) + tag + data
        return out + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)

    ihdr = struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0)
    body = (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", ihdr)
        + chunk(b"IDAT", zlib.compress(raw, 9))
        + chunk(b"IEND", b"")
    )
    io.open(path, "wb").write(body)


def hsv_to_rgb(h, s, v):
    i = int(h * 6) % 6
    f = h * 6 - int(h * 6)
    p, q, t = v * (1 - s), v * (1 - f * s), v * (1 - (1 - f) * s)
    r, g, b = [(v, t, p), (q, v, p), (p, v, t), (p, q, v), (t, p, v), (v, p, q)][i]
    return int(r * 255), int(g * 255), int(b * 255)


def build_thumbnail_pool(rng, directory, count):
    directory.mkdir(parents=True, exist_ok=True)
    paths = []
    for index in range(count):
        path = directory / f"seed_{index:02d}.png"
        if not path.exists():
            hue = rng.random()
            top = hsv_to_rgb(hue, 0.55, 0.42)
            bottom = hsv_to_rgb((hue + 0.06) % 1.0, 0.6, 0.12)
            stripe = hsv_to_rgb((hue + 0.5) % 1.0, 0.5, 0.75)
            make_png(path, 320, 240, top, bottom, stripe, rng.choice([40, 64, 90, 120]))
        paths.append(path)
    return paths


def canonical_manifest_hash(entries):
    manifest = {"entries": entries}
    payload = json.dumps(manifest, separators=(",", ":"), ensure_ascii=False)
    return hashlib.sha256(payload.encode("utf-8")).hexdigest()


class BlobPool:
    """Shared content payloads; dedup keeps the object store small."""

    def __init__(self, rng, client, profile, size):
        self.rng = rng
        self.client = client
        self.profile = profile
        self.blobs = []
        for index in range(size):
            body = (f"ads seed blob {index:05d}\n" * rng.randint(4, 160)).encode("utf-8")
            self.blobs.append((hashlib.sha256(body).hexdigest(), len(body), body))
        self.uploaded = set()
        self.upload_count = 0

    def pick(self):
        sha256, size, body = self.rng.choice(self.blobs)
        if sha256 not in self.uploaded:
            self.client.upload_object(sha256, body, profile=self.profile)
            self.uploaded.add(sha256)
            self.upload_count += 1
        return sha256, size


def asset_name(rng, kind, used):
    pools = {
        "creature": CREATURES,
        "vehicle": VEHICLES,
        "prop": NOUNS,
        "env": NOUNS,
    }
    for _ in range(200):
        name = f"{rng.choice(ADJECTIVES)}_{rng.choice(pools[kind])}"
        if rng.random() < 0.35:
            name += f"_{rng.randint(1, 99):02d}"
        if name not in used:
            used.add(name)
            return name
    name = f"asset_{len(used):05d}"
    used.add(name)
    return name


def run_cli(executable, arguments):
    import subprocess

    completed = subprocess.run(
        [str(Path(executable).resolve()), *arguments],
        capture_output=True,
        text=True,
        check=False,
    )
    if completed.returncode != 0:
        raise RuntimeError(
            f"ads {' '.join(arguments[:2])} failed: {completed.stderr.strip()}"
        )
    return completed.stdout.strip()


def seed_wip(args):
    """Seeds live WIP streams by driving the real v8 workflow via the CLI.

    For each sampled department: register a series of WIP micro-versions from
    a mutating work folder, promote the head once or twice along the way, and
    leave trailing unpromoted WIPs. The first departments exceed the default
    GC retention (20) so `ads gc --dry-run` has something to report.
    """

    import tempfile
    from pathlib import Path as _Path

    rng = random.Random(args.seed + 1000)
    started = time.time()
    used_names = set()
    departments_seeded = 0
    wips_registered = 0
    promotes = 0
    pins = 0

    for index in range(args.wip_departments):
        category = weighted_choice(rng, [(c, w) for c, _, w in CATEGORIES])
        kind = next(k for c, k, _ in CATEGORIES if c == category)
        code = asset_name(rng, kind, used_names)
        department = weighted_choice(rng, DEPARTMENTS[kind])
        common = [
            "--store", str(args.wip_store),
            "--category", category,
            "--asset-code", code,
            "--department", department,
        ]

        # The first two departments exceed the default GC retention of 20.
        wip_count = 25 if index < 2 else weighted_choice(
            rng, [(3, 5), (5, 4), (8, 3), (12, 2), (18, 1)])
        promote_count = min(rng.randint(1, 2), max(1, wip_count - 2))
        promote_points = sorted(
            rng.sample(range(2, wip_count), promote_count)) if wip_count > 2 else []

        department_promotes = 0
        with tempfile.TemporaryDirectory() as temp:
            work = _Path(temp)
            file_names = [f"{code}.usd", f"geo/{code}_geo.usd"][: rng.randint(1, 2)]
            for seq in range(1, wip_count + 1):
                target = work / rng.choice(file_names)
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_text(
                    f"{code} {department} wip {seq}\n" * rng.randint(2, 30),
                    encoding="utf-8",
                )
                run_cli(args.ads, ["wip", "add", *common, "--source", str(work)])
                wips_registered += 1
                if seq in promote_points:
                    run_cli(args.ads, ["publish", "promote", *common])
                    promotes += 1
                    department_promotes += 1

        if department_promotes >= 2 and rng.random() < 0.5:
            run_cli(args.ads, ["current", "set", *common, "--version", "1"])
            pins += 1

        departments_seeded += 1
        if departments_seeded % 10 == 0:
            print(f"{departments_seeded}/{args.wip_departments} departments "
                  f"({wips_registered} wips, {time.time() - started:.0f}s)")

    print("wip seed complete:")
    print(f"  departments: {departments_seeded}")
    print(f"  wips:        {wips_registered}")
    print(f"  promotes:    {promotes}")
    print(f"  pins:        {pins}")
    print(f"  elapsed:     {time.time() - started:.1f}s")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--server", default="http://127.0.0.1:8799")
    parser.add_argument("--token", default="demo-token")
    parser.add_argument("--profile", default="main")
    parser.add_argument("--target", type=int, default=3200,
                        help="number of category/asset/department entries")
    parser.add_argument("--seed", type=int, default=8)
    parser.add_argument("--thumb-dir", default="target/webapp_tmp/seed_thumbs")
    parser.add_argument("--wip-store",
                        help="seed WIP streams via the ads CLI against this store "
                             "(requires ads serve to be stopped); skips the HTTP phase")
    parser.add_argument("--ads", default="target/debug/ads.exe",
                        help="ads executable used by --wip-store")
    parser.add_argument("--wip-departments", type=int, default=40)
    args = parser.parse_args()

    if args.wip_store:
        seed_wip(args)
        return

    rng = random.Random(args.seed)
    client = AdsHttpClient(args.server, token=args.token, timeout=60.0)
    client.profiles()  # fail fast on connection/token problems

    blob_pool = BlobPool(rng, client, args.profile, size=1000)
    thumbnails = build_thumbnail_pool(rng, Path(args.thumb_dir), count=48)

    started = time.time()
    used_names = set()
    entries_created = 0
    versions_created = 0
    thumbs_set = 0
    pins_set = 0

    while entries_created < args.target:
        category = weighted_choice(rng, [(c, w) for c, _, w in CATEGORIES])
        kind = next(k for c, k, _ in CATEGORIES if c == category)
        code = asset_name(rng, kind, used_names)

        department_count = weighted_choice(rng, [(1, 5), (2, 4), (3, 3), (4, 1)])
        departments = []
        for dept, _ in sorted(DEPARTMENTS[kind], key=lambda item: -item[1]):
            if len(departments) >= department_count:
                break
            departments.append(dept)

        # Per-department version history with mostly-stable files so dedup
        # behaves like real production data.
        for department in departments:
            if entries_created >= args.target:
                break
            version_count = weighted_choice(
                rng, [(1, 7), (2, 6), (3, 5), (4, 3), (5, 2), (7, 1), (12, 1)])
            file_templates = rng.sample(FILE_KINDS, rng.randint(1, 5))
            files = {}
            for template, _kind in file_templates:
                files[template.format(code=code)] = blob_pool.pick()

            day = rng.randint(0, 540)
            wip_seq = 0
            for version in range(1, version_count + 1):
                if version > 1:
                    for path in rng.sample(list(files), max(1, len(files) // 3)):
                        files[path] = blob_pool.pick()
                    day += rng.randint(0, 21)

                manifest_entries = [
                    {"relative_path": path, "sha256": sha, "size": size, "mode": 420}
                    for path, (sha, size) in sorted(files.items())
                ]
                created_at = (
                    f"2025-01-06T{rng.randint(8, 19):02d}:{rng.randint(0, 59):02d}:00+00:00"
                )
                # Schema v8: most versions are promotions of WIP micro-versions
                # (metadata-only, staging source paths, promoted_from set); the
                # rest are direct `add --source` registrations.
                promoted = rng.random() < 0.6
                if promoted:
                    wip_seq += rng.randint(1, 6)
                    source_path = (
                        f"D:/workspace/.ads-staging/{rng.randrange(16 ** 8):08x}"
                        f"/{category}/{code}/{department}"
                    )
                else:
                    source_path = rng.choice(
                        [
                            f"D:/delivery/{code}_{department}_fix",
                            f"D:/work/{code}/{department}/out",
                        ]
                    )
                record = {
                    "department_key": {
                        "asset_key": {"category": category, "asset_code": code},
                        "department": department,
                    },
                    "version": version,
                    "manifest_hash": canonical_manifest_hash(manifest_entries),
                    "created_at": created_at.replace(
                        "2025-01-06", offset_date(day)),
                    "source_path": source_path,
                    "file_count": len(manifest_entries),
                    "total_bytes": sum(e["size"] for e in manifest_entries),
                }
                if promoted:
                    record["promoted_from"] = wip_seq
                client.import_version_info(
                    {"version": record, "manifest": {"entries": manifest_entries}},
                    profile=args.profile,
                )
                versions_created += 1

            if rng.random() < 0.65:
                thumb = thumbnails[hash((category, code, department)) % len(thumbnails)]
                client.upload_thumbnail(
                    thumb,
                    profile=args.profile,
                    category=category,
                    asset_code=code,
                    department=department,
                    version=version_count,
                )
                thumbs_set += 1

            if version_count >= 2 and rng.random() < 0.12:
                client.put_json("/api/current", {
                    "profile": args.profile,
                    "category": category,
                    "asset_code": code,
                    "department": department,
                    "version": rng.randint(1, version_count - 1),
                })
                pins_set += 1

            entries_created += 1
            if entries_created % 200 == 0:
                elapsed = time.time() - started
                print(f"{entries_created}/{args.target} entries "
                      f"({versions_created} versions, {elapsed:.0f}s)")

    elapsed = time.time() - started
    print("seed complete:")
    print(f"  entries:    {entries_created}")
    print(f"  versions:   {versions_created}")
    print(f"  objects:    {blob_pool.upload_count} uploaded (shared pool)")
    print(f"  thumbnails: {thumbs_set}")
    print(f"  pins:       {pins_set}")
    print(f"  elapsed:    {elapsed:.1f}s")


def offset_date(days):
    # Deterministic calendar walk from 2025-01-06 without datetime arithmetic
    # surprises: precomputed month lengths for 2025-2026.
    months = [
        ("2025", [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]),
        ("2026", [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]),
    ]
    day_of_year = 5 + days  # Jan 6 is day index 5
    for year, lengths in months:
        for month, length in enumerate(lengths, start=1):
            if day_of_year < length:
                return f"{year}-{month:02d}-{day_of_year + 1:02d}"
            day_of_year -= length
    return "2026-12-28"


if __name__ == "__main__":
    main()
