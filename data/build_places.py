"""Build data/israel-places.tsv.gz: places that are not streets, up to 200 per locality.

Source: OpenStreetMap through the Overpass API (© OpenStreetMap contributors, ODbL 1.0,
https://www.openstreetmap.org/copyright). Inputs, in one directory:

  places.json   node["place"~"^(city|town|village|hamlet|isolated_dwelling)$"] in IL and PS; out;
  p_*.json      named places by category (tourism, amenity, shop, railway, leisure, office,
                healthcare, historic, landuse, place=neighbourhood, named buildings); out center tags;

Each place goes to the locality its address names, else to the nearest locality weighted by
size (Jerusalem reaches kilometres further than a moshav). Only localities of the streets
list are kept, under that list's name. A locality keeps its 200 most useful places:
hospitals, malls, stations, institutions and neighbourhoods before schools and buildings.

  python data/build_places.py <download dir>
"""

import gzip
import json
import math
import re
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
PER_LOCALITY = 200

HEBREW = re.compile(r"[א-ת]")
FINALS = str.maketrans("ךםןףץ", "כמנפצ")


def norm(s):
    s = s.translate(FINALS)
    s = re.sub(r"[\s\-־,]+", " ", s)
    s = "".join(c for c in s if c.isalnum() or c == " ")
    return re.sub(" +", " ", s).strip()


def variants(name):
    """Spellings of a locality name to match the streets list ("קריית" / "קרית")."""
    n = norm(name)
    yield n
    yield n.replace("קריית", "קרית")
    yield n.replace("קרית", "קריית")
    yield n.replace(" ", "")


def score(tags):
    """How likely a caller names this place."""
    top = {
        "amenity": {"hospital", "university", "bus_station", "conference_centre", "townhall", "courthouse", "ferry_terminal"},
        "shop": {"mall", "department_store"},
        "railway": {"station"},
        "public_transport": {"station"},
        "aeroway": {"aerodrome", "terminal"},
        "leisure": {"stadium", "water_park"},
        "tourism": {"museum", "zoo", "theme_park", "aquarium"},
        "office": {"government"},
        "place": {"neighbourhood", "suburb", "quarter"},
    }
    mid = {
        "amenity": {"college", "police", "community_centre", "theatre", "cinema", "library", "marketplace", "clinic",
                    "arts_centre", "events_venue", "social_facility", "nursing_home"},
        "tourism": {"hotel", "attraction", "gallery", "hostel", "guest_house"},
        "shop": {"supermarket"},
        "leisure": {"park", "sports_centre", "marina", "beach_resort", "garden"},
        "natural": {"beach"},
        "landuse": {"retail", "industrial", "commercial"},
        "healthcare": {"hospital", "clinic", "centre", "rehabilitation"},
        "railway": {"halt"},
    }
    s = 40
    if any(tags.get(k) in v for k, v in top.items()):
        s = 100
    elif any(tags.get(k) in v for k, v in mid.items()) or "historic" in tags:
        s = 70
    if "wikidata" in tags or "wikipedia" in tags:
        s += 20
    if "addr:street" in tags:
        s += 10
    return s


def hebrew_names(tags):
    names = []
    for key in ("name:he", "name", "alt_name:he", "alt_name", "short_name:he", "short_name", "old_name:he", "official_name:he"):
        for n in tags.get(key, "").split(";"):
            n = n.strip()
            if n and HEBREW.search(n) and n not in names:
                names.append(n)
    return names


GENERIC = ["בית החולים", "בית חולים", 'בי"ח', "ביה\"ח", "המרכז הרפואי", "מרכז רפואי", "קניון", "מרכז", "מגדלי", "מגדל",
           "תחנת הרכבת", "תחנת רכבת", "תחנת", "אוניברסיטת", "האוניברסיטה", "מכללת", "המכללה", "בית הספר", "בית ספר",
           "בית הכנסת", "בית כנסת", "פארק", "גן", "שכונת", "אצטדיון", "היכל", "מלון", "מוזיאון"]


def short_names(name, town):
    """What callers say: "איכילוב" for 'בי"ח איכילוב', "סבידור מרכז" for "תל אביב סבידור מרכז"."""
    out = []
    # The locality as a whole, and its first part ("תל אביב" of "תל אביב - יפו").
    for t in dict.fromkeys([town, town.split(" - ")[0].strip()]):
        for sep in (" ", " - ", "-"):
            if name.startswith(t + sep):
                out.append(name[len(t) + len(sep):].strip(" -"))
            if name.endswith(sep + t):
                out.append(name[: -len(t) - len(sep)].strip(" -"))
    for base in [name] + out:
        for g in GENERIC:
            if base.startswith(g + " ") and len(base) - len(g) > 3:
                out.append(base[len(g) + 1:].strip(" -"))
    return [o for o in dict.fromkeys(out) if len(norm(o)) >= 3 and o != name]


def km(a, b):
    lat = math.radians((a[0] + b[0]) / 2)
    return math.hypot((a[0] - b[0]) * 111.32, (a[1] - b[1]) * 111.32 * math.cos(lat))


def main(src):
    src = Path(src)
    # The streets list's localities, by every spelling.
    official = {}
    with gzip.open(HERE / "israel-streets.tsv.gz", "rt", encoding="utf-8") as f:
        for line in f:
            if line.startswith("#"):
                continue
            city = line.split("\t")[1].strip()
            for v in variants(city):
                official.setdefault(v, city)

    def locality(name):
        for v in variants(name):
            if v in official:
                return official[v]
        return None

    # Localities with a point and a reach.
    towns = []
    for e in json.loads((src / "places.json").read_text(encoding="utf-8"))["elements"]:
        t = e.get("tags", {})
        name = locality(t.get("name:he") or t.get("name", ""))
        if not name:
            continue
        try:
            pop = float(t.get("population", "").replace(",", ""))
        except ValueError:
            pop = 0
        default = {"city": 6.0, "town": 2.5, "village": 1.0}.get(t["place"], 0.6)
        reach = max(0.8, 0.25 * math.sqrt(pop / 1000)) if pop else default
        towns.append((name, (e["lat"], e["lon"]), reach))

    def nearest(point):
        best = min(towns, key=lambda t: km(point, t[1]) / t[2])
        return best[0] if km(point, best[1]) / best[2] <= 1.3 else None

    by_town = {}
    seen = set()
    for part in sorted(src.glob("p_*.json")):
        try:
            elements = json.loads(part.read_text(encoding="utf-8"))["elements"]
        except (ValueError, KeyError):
            print(f"skipping {part.name}: not an Overpass answer", file=sys.stderr)
            continue
        for e in elements:
            if (e["type"], e["id"]) in seen:
                continue
            seen.add((e["type"], e["id"]))
            t = e.get("tags", {})
            names = hebrew_names(t)
            point = (e["lat"], e["lon"]) if "lat" in e else (e["center"]["lat"], e["center"]["lon"]) if "center" in e else None
            if not names or not point:
                continue
            town = locality(t.get("addr:city", "")) or nearest(point)
            if not town:
                continue
            by_town.setdefault(town, []).append((score(t), names, t.get("addr:street", ""), t.get("addr:housenumber", ""), point))

    rows = []
    for town, places in by_town.items():
        kept = {}
        for p in sorted(places, key=lambda p: -p[0]):
            key = norm(p[1][0])
            if key not in kept and norm(town) != key:
                kept[key] = p
            if len(kept) == PER_LOCALITY:
                break
        for s, names, street, number, (lat, lon) in kept.values():
            names = list(dict.fromkeys(names + [a for n in names for a in short_names(n, town)]))
            clean = lambda x: re.sub(r"[\t|\r\n]+", " ", x).strip()
            rows.append("\t".join([town, clean(names[0]), "|".join(clean(n) for n in names[1:]), clean(street), clean(number), f"{lat:.5f}", f"{lon:.5f}"]))

    out = HERE / "israel-places.tsv.gz"
    with gzip.open(out, "wt", encoding="utf-8", compresslevel=9) as f:
        f.write("# Places that are not streets, up to 200 per locality. © OpenStreetMap contributors, ODbL 1.0 (https://www.openstreetmap.org/copyright).\n")
        f.write("# Built by data/build_places.py from the Overpass API.\n")
        f.write("# locality\tname\taliases (|)\tstreet\thouse number\tlat\tlon\n")
        f.write("\n".join(sorted(rows)) + "\n")
    print(f"{len(rows)} places in {len(by_town)} localities -> {out} ({out.stat().st_size // 1024} KB)")


if __name__ == "__main__":
    main(sys.argv[1] if len(sys.argv) > 1 else ".")
