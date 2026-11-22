#!/usr/bin/env python3
"""Generates the window's icons:

  crates/kiri/src/ui/brands.rs  brand logos for commonly containerized images
                                (Simple Icons, CC0), compiled into the binary.
                                The engine sends only the logos for images
                                you have, so the page never carries the catalog.
  crates/kiri/src/ui/icons.js   the few UI icons the page draws (Phosphor, MIT).

Nothing is fetched at runtime.

    python3 scripts/ui-icons.py
"""
import json, os, re, sys, urllib.request

SIMPLE_ICONS = "16.35.0"
PHOSPHOR = "2.1.1"

# Image repository name (last path component, or a full path) -> Simple Icons slug.
CATALOG = {
    # base OS images
    "alpine": "alpinelinux", "ubuntu": "ubuntu", "debian": "debian", "fedora": "fedora",
    "centos": "centos", "rockylinux": "rockylinux", "almalinux": "almalinux", "archlinux": "archlinux",
    "amazonlinux": "amazonwebservices", "opensuse": "opensuse", "gentoo": "gentoo", "nixos": "nixos",
    "busybox": None, "distroless": "google",
    # web servers and proxies
    "nginx": "nginx", "nginx-unprivileged": "nginx", "httpd": "apache", "caddy": "caddy",
    "traefik": "traefikproxy", "haproxy": None, "envoy": "envoyproxy", "kong": "kong",
    "tomcat": "apachetomcat", "jetty": "eclipsejetty",
    # databases and caches
    "redis": "redis", "redis-stack": "redis", "valkey": "valkey", "memcached": None,
    "postgres": "postgresql", "postgis": "postgresql", "timescaledb": "timescale",
    "mysql": "mysql", "mariadb": "mariadb", "percona": "percona", "mongo": "mongodb", "mongodb": "mongodb",
    "cassandra": "apachecassandra", "couchdb": "apachecouchdb", "couchbase": "couchbase",
    "neo4j": "neo4j", "influxdb": "influxdb", "clickhouse": "clickhouse", "cockroach": "cockroachlabs",
    "elasticsearch": "elasticsearch", "opensearch": "opensearch", "solr": "apachesolr", "meilisearch": "meilisearch",
    "typesense": None, "qdrant": "qdrant", "etcd": "etcd", "sqlite": "sqlite", "surrealdb": "surrealdb",
    "dragonfly": None, "keydb": None, "scylla": "scylladb", "arangodb": "arangodb", "supabase": "supabase",
    # messaging and streaming
    "rabbitmq": "rabbitmq", "kafka": "apachekafka", "zookeeper": None, "nats": "natsdotio",
    "pulsar": "apachepulsar", "activemq": None, "emqx": None, "mosquitto": "eclipsemosquitto",
    "redpanda": None,
    # Confluent Platform: ZooKeeper has no single-colour mark, so its image shows Confluent's
    "cp-kafka": "confluent", "cp-zookeeper": "confluent", "cp-server": "confluent",
    "cp-schema-registry": "confluent", "cp-kafka-connect": "confluent", "cp-kafka-rest": "confluent",
    "cp-ksqldb-server": "confluent", "cp-enterprise-control-center": "confluent",
    # Temporal: by full path, since "server" and "ui" alone name nothing
    "temporalio/auto-setup": "temporal", "temporalio/server": "temporal", "temporalio/admin-tools": "temporal",
    "temporalio/ui": "temporal", "temporalio/temporal": "temporal",
    # languages and runtimes
    "python": "python", "node": "nodedotjs", "golang": "go", "rust": "rust", "openjdk": "openjdk",
    "eclipse-temurin": "eclipseadoptium", "amazoncorretto": "openjdk", "php": "php", "ruby": "ruby",
    "perl": "perl", "dotnet": "dotnet", "aspnet": "dotnet", "deno": "deno", "bun": "bun",
    "elixir": "elixir", "erlang": "erlang", "haskell": "haskell", "julia": "julia", "r-base": "r",
    "swift": "swift", "gcc": "gnu", "clojure": "clojure", "kotlin": "kotlin", "scala": "scala",
    "gradle": "gradle", "maven": "apachemaven", "composer": "composer",
    # observability
    "grafana": "grafana", "prometheus": "prometheus", "alertmanager": "prometheus", "loki": "grafana",
    "tempo": "grafana", "jaeger": None, "kibana": "kibana", "logstash": "logstash", "fluentd": "fluentd",
    "fluent-bit": "fluentbit", "telegraf": "influxdb", "otel": "opentelemetry",
    "opentelemetry-collector": "opentelemetry", "zipkin": None, "sentry": "sentry",
    # dev and ops tools
    "jenkins": "jenkins", "gitlab-ce": "gitlab", "gitlab-runner": "gitlab", "gitea": "gitea",
    "sonarqube": "sonarqubeserver", "vault": "vault", "consul": "consul", "nomad": "nomad",
    "terraform": "terraform", "registry": "docker", "docker": "docker", "dind": "docker",
    "portainer": "portainer", "minio": "minio", "localstack": "localstack", "keycloak": "keycloak",
    "nexus3": "sonatype", "argocd": "argo", "vaultwarden": "vaultwarden", "harbor": "harbor",
    "kubectl": "kubernetes", "k3s": "k3s", "kindest": "kubernetes", "node-exporter": "prometheus",
    "adminer": None, "phpmyadmin": "phpmyadmin", "pgadmin4": "postgresql", "mailhog": None,
    "mailpit": None, "swagger-ui": "swagger", "n8n": "n8n", "airflow": "apacheairflow",
    "spark": "apachespark", "flink": "apacheflink", "hadoop": "apachehadoop", "jupyter": "jupyter",
    "ollama": "ollama", "pytorch": "pytorch", "tensorflow": "tensorflow",
    # apps
    "wordpress": "wordpress", "ghost": "ghost", "drupal": "drupal", "joomla": "joomla",
    "nextcloud": "nextcloud", "mediawiki": "mediawiki", "matomo": "matomo", "mattermost": "mattermost",
    "rocket.chat": "rocketdotchat", "home-assistant": "homeassistant", "homeassistant": "homeassistant",
    "jellyfin": "jellyfin", "plex": "plex", "pihole": "pihole", "metabase": "metabase",
    "superset": "apachesuperset", "directus": "directus", "strapi": "strapi", "odoo": "odoo",
    "discourse": "discourse", "redmine": "redmine", "uptime-kuma": "uptimekuma",
}

# Phosphor icons the window uses, by our name -> Phosphor name.
PHOSPHOR_ICONS = {"stack": "stack", "cube": "cube", "database": "database", "network": "share-network", "gear": "gear-six", "textaa": "text-aa", "note": "note", "notepad": "notepad", "notepencil": "note-pencil", "pencil": "pencil-simple", "info": "info", "globe": "globe-simple", "chatdots": "chat-circle-dots", "sparkle": "sparkle", "wrench": "wrench", "shieldcheck": "shield-check", "trendup": "trend-up", "dot": "dot-outline", "hammer": "hammer", "checkcircle": "check-circle", "xcircle": "x-circle", "lightning": "lightning", "warning": "warning", "clock": "clock", "calendar": "calendar-blank", "steps": "stack-simple", "expand": "arrows-out-simple", "collapse": "arrows-in-simple", "minus": "minus-circle", "copy": "copy", "check": "check", "pods": "hexagon", "services": "plugs-connected", "sun": "sun", "moon": "moon", "link": "link-simple", "github": "github-logo", "sliders": "sliders", "play": "play", "open": "arrow-square-out", "pull": "download-simple", "trash": "trash", "logs": "article", "stop": "stop", "restart": "arrow-clockwise", "bug": "bug", "folder": "folder-open", "briefcase": "briefcase", "package": "package", "rocket": "rocket", "code": "code", "send": "paper-plane-tilt", "caret": "caret-right", "eye": "eye", "eyeoff": "eye-slash", "lock": "lock", "regen": "arrows-clockwise", "pause": "pause", "sidebar": "sidebar-simple", "broom": "broom", "updown": "arrows-down-up", "warncircle": "warning-circle", "confetti": "confetti", "starfill": "star-fill", "list": "list", "squares": "squares-four", "usb": "usb", "plug": "plug", "eject": "eject", "mouse": "mouse-simple", "keyboard": "keyboard", "ethernet": "network", "circuitry": "circuitry", "broadcast": "broadcast", "harddrive": "hard-drive", "microphone": "microphone", "webcam": "webcam",
                  # Project picker (create modal)
                  "terminal": "terminal", "desktop": "desktop", "browser": "browser", "cloud": "cloud",
                  "gitbranch": "git-branch", "cpu": "cpu", "tree": "tree-structure", "appwindow": "app-window",
                  "flask": "flask", "atom": "atom", "robot": "robot", "puzzle": "puzzle-piece",
                  "paintbrush": "paint-brush", "camera": "camera", "music": "music-notes", "gamepad": "game-controller",
                  "storefront": "storefront", "users": "users", "chart": "chart-line-up", "mappin": "map-pin",
                  "house": "house", "car": "car", "airplane": "airplane", "leaf": "leaf", "fire": "fire",
                  "drop": "drop", "planet": "planet", "magicwand": "magic-wand", "anchor": "anchor",
                  "coffee": "coffee", "book": "book", "heart": "heart", "star": "star", "flower": "flower",
                  "boat": "boat", "bike": "bicycle", "palette": "palette", "megaphone": "megaphone",
                  "bell": "bell", "key": "key", "wallet": "wallet", "shopping": "shopping-bag",
                  "firstaid": "first-aid", "gradcap": "graduation-cap", "lightbulb": "lightbulb",
                  "fingerprint": "fingerprint", "compass": "compass", "flag": "flag", "trophy": "trophy",
                  "timer": "timer", "infinity": "infinity", "diamond": "diamond", "circles": "circles-three-plus",
                  "train": "train", "cloudarrow": "cloud-arrow-up", "buildings": "buildings"}

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
OUT_JS = os.path.join(ROOT, "crates/kiri/src/ui/icons.js")
OUT_RS = os.path.join(ROOT, "crates/kiri/src/ui/brands.rs")


def get(url):
    with urllib.request.urlopen(url, timeout=30) as r:
        return r.read().decode()


# Logos above this size (after minifying) fall back to the stack icon, unless
# the image is one people run all the time.
MAX_BYTES = 2500
ALWAYS = {"postgresql", "mysql", "kubernetes", "jenkins", "rust", "grafana", "debian", "apachekafka",
          "apache", "ollama", "mongodb", "elasticsearch"}

# Logos taken from an older Simple Icons, with that release's colour: the mark
# people know, where a rebrand changed it. Redis: the stacked cubes (11.15.0),
# not the 2024 "R".
PINNED = {"redis": ("11.15.0", "DC382D")}

# Logos Simple Icons lacks, as (colour, path) already in the 24-unit box.
# Confluent: the mark from confluent.io's header logo, scaled from its 36-unit box.
LOCAL = {"confluent": ("173361", "M20.3 3.7C18.6 2 16.5.9 14.2.5C11.9 0 9.5.2 7.4 1.1C5.2 2 3.3 3.6 2 5.5C.7 7.5 0 9.8 0 12.1C0 15.3 1.3 18.3 3.5 20.5C5.8 22.8 8.7 24 11.9 24C14.2 24 16.5 23.3 18.5 22C20.4 20.7 21.9 18.8 22.8 16.7C23.7 14.5 24 12.1 23.5 9.8C23 7.5 21.9 5.4 20.3 3.7L20.3 3.7ZM22.1 14.2C21.7 16.2 20.8 18.1 19.3 19.5C17.8 21 16 22 13.9 22.4C11.9 22.8 9.8 22.6 7.9 21.8C6 21 4.3 19.7 3.2 18C2 16.2 1.4 14.2 1.4 12.1C1.4 9.3 2.5 6.7 4.5 4.7C6.5 2.7 9.1 1.6 11.9 1.6C14 1.6 16 2.2 17.7 3.4C19.4 4.5 20.8 6.2 21.6 8.1C22.3 10 22.6 12.1 22.1 14.2ZM17.9 11.8C17.7 11.7 17.5 11.7 17.3 11.7L15.5 11.7C14.6 11.7 13.6 11.6 12.4 11.6C12.4 10.7 12.4 9.6 12.3 8.5L12.3 6.7C12.3 6.5 12.3 6.3 12.3 6.1C12.2 5.7 12.2 5.3 12.2 4.9L12.2 4.9H11.6L11.6 4.9C11.6 5.3 11.5 5.7 11.5 6.1C11.5 6.3 11.5 6.5 11.5 6.7L11.4 8.5C11.4 8.9 11.4 9.3 11.4 9.8L11.3 9.6C11.2 9.3 11.1 8.9 10.9 8.6L10.2 7C10.1 6.9 10.1 6.7 10 6.6L9.9 6.4C9.8 6.1 9.6 5.7 9.4 5.4L9.4 5.3L8.9 5.6L8.9 5.6C9 6 9.2 6.4 9.3 6.8L9.3 6.8C9.4 7 9.4 7.1 9.5 7.3L10.1 9C10.2 9.3 10.4 9.7 10.6 10.1C10.4 10 10.3 9.9 10.2 9.7C10 9.6 9.8 9.4 9.7 9.3L8.4 8C8.2 7.9 8.1 7.8 7.9 7.6C7.6 7.3 7.3 7.1 7.1 6.8L7 6.8L6.6 7.2L6.6 7.2C6.8 7.5 7.1 7.8 7.3 8C7.5 8.2 7.7 8.4 7.8 8.6L9.1 9.9C9.4 10.2 9.7 10.5 9.9 10.8L9.8 10.7C9.5 10.6 9.1 10.4 8.8 10.3L7.1 9.7L6.6 9.5C6.2 9.4 5.8 9.2 5.4 9.1L5.4 9.1L5.1 9.6L5.2 9.6C5.5 9.8 5.8 10 6.1 10.1L6.2 10.1C6.3 10.2 6.5 10.3 6.6 10.3L8.4 11.1C8.8 11.3 9.1 11.4 9.4 11.6L9.6 11.6C9.1 11.6 8.7 11.6 8.3 11.7L6.5 11.7C6.3 11.7 6.1 11.7 5.9 11.7C5.5 11.8 5.1 11.8 4.7 11.8L4.7 11.8V12.4L4.7 12.4C5.1 12.4 5.6 12.5 5.9 12.5C6.1 12.5 6.3 12.5 6.5 12.5L8.3 12.5C9.2 12.6 10.1 12.6 11 12.6H11.4C11.4 13.6 11.4 14.7 11.5 15.7L11.5 17.5C11.5 17.7 11.5 17.9 11.6 18.1C11.6 18.5 11.6 18.9 11.6 19.3L11.6 19.3H12.2L12.2 19.3C12.2 18.9 12.2 18.5 12.2 18.1C12.3 17.9 12.3 17.7 12.3 17.5L12.3 15.7C12.3 15.3 12.3 14.8 12.4 14.4L12.4 14.5C12.5 14.9 12.7 15.2 12.9 15.6L13.6 17.2C13.7 17.4 13.8 17.6 13.9 17.8C14 18.1 14.2 18.5 14.4 18.8L14.4 18.9L14.9 18.7L14.9 18.6C14.8 18.3 14.7 18 14.5 17.6L14.5 17.4C14.4 17.3 14.4 17.1 14.3 16.9L13.7 15.2C13.5 14.8 13.4 14.4 13.2 14.1L13.3 14.2C13.6 14.4 13.8 14.7 14.1 14.9L15.4 16.2C15.6 16.3 15.7 16.4 15.8 16.5C16.1 16.8 16.4 17.1 16.8 17.4L16.8 17.4L17.2 17L17.2 17C16.9 16.7 16.6 16.4 16.3 16.1L16.3 16C16.2 15.9 16.1 15.8 16 15.6L14.7 14.3C14.5 14 14.2 13.7 14 13.5L13.9 13.4C14 13.4 14.1 13.5 14.2 13.5C14.4 13.6 14.7 13.8 15 13.9L16.7 14.5C16.9 14.6 17.1 14.6 17.3 14.7C17.7 14.9 18.1 15 18.4 15.1L18.5 15.1L18.7 14.6L18.6 14.6C18.3 14.4 17.9 14.2 17.6 14.1L17.5 14.1C17.4 14 17.2 13.9 17 13.8L15.4 13.1C15 12.9 14.7 12.7 14.2 12.6C14.6 12.6 15.1 12.5 15.5 12.5L17.3 12.5C17.5 12.5 17.7 12.5 17.9 12.4H17.9C18.3 12.4 18.7 12.4 19.1 12.4L19.1 12.4V11.8L19.1 11.8C18.7 11.8 18.3 11.8 17.9 11.8L17.9 11.8Z")}


def path_of(svg):
    paths = re.findall(r'<path[^>]* d="([^"]+)"', svg)
    if not paths:
        sys.exit(f"no path in {svg[:80]}")
    return minify(" ".join(paths))


ARGS = {"m": 2, "l": 2, "h": 1, "v": 1, "c": 6, "s": 4, "q": 4, "t": 2, "a": 7, "z": 0}
NUM = re.compile(r"[-+]?(?:\d+\.?\d*|\.\d+)(?:[eE][-+]?\d+)?")


def minify(d, places=1):
    """Rounds coordinates and drops redundant characters. Arc flags are read
    one character at a time, since optimisers write them run together."""
    out, i, cmd, n = [], 0, None, 0
    prev_num = None  # text of the last number written, for separator choice

    def emit_num(v):
        nonlocal prev_num
        t = f"{round(v, places):.{places}f}".rstrip("0").rstrip(".")
        if t in ("-0", ""):
            t = "0"
        if t.startswith("0.") : t = t[1:]
        elif t.startswith("-0."): t = "-" + t[2:]
        if prev_num is not None and not t.startswith("-") and not (t.startswith(".") and "." in prev_num):
            out.append(" ")
        out.append(t)
        prev_num = t

    while i < len(d):
        ch = d[i]
        if ch.isalpha():
            cmd, n = ch, 0
            out.append(ch)
            prev_num = None
            i += 1
            continue
        if ch in " ,\n\t":
            i += 1
            continue
        if cmd is None:
            sys.exit(f"path starts with a number: {d[:40]}")
        k = n % ARGS[cmd.lower()] if ARGS[cmd.lower()] else 0
        if cmd.lower() == "a" and k in (3, 4):
            # A flag: exactly one character, 0 or 1.
            if prev_num is not None:
                out.append(" ")
            out.append(d[i])
            prev_num = d[i]
            i += 1
        else:
            m = NUM.match(d, i)
            if not m:
                sys.exit(f"bad path near {d[i:i+20]!r}")
            emit_num(float(m.group()))
            i = m.end()
        n += 1
    return "".join(out)


def main():
    data = json.loads(get(f"https://unpkg.com/simple-icons@{SIMPLE_ICONS}/data/simple-icons.json"))
    by_slug = {i["slug"]: i for i in data}
    brands, missing, too_big = {}, [], {}
    for image, slug in CATALOG.items():
        if slug is None:
            continue
        if slug in LOCAL:
            brands[slug] = list(LOCAL[slug])
            continue
        if slug not in by_slug:
            missing.append(f"{image}->{slug}")
            continue
        if slug not in brands and slug not in too_big:
            version, hex = PINNED.get(slug, (SIMPLE_ICONS, by_slug[slug]["hex"]))
            svg = get(f"https://unpkg.com/simple-icons@{version}/icons/{slug}.svg")
            d = path_of(svg)
            if len(d) > MAX_BYTES and slug not in ALWAYS:
                too_big[slug] = len(d)
                continue
            brands[slug] = [hex, d]
    images = {k: v for k, v in CATALOG.items() if v in brands}
    ui = {name: path_of(get(f"https://unpkg.com/@phosphor-icons/core@{PHOSPHOR}/assets/regular/{ph}.svg"))
          for name, ph in PHOSPHOR_ICONS.items()}

    with open(OUT_JS, "w") as f:
        f.write(f"/* Generated by scripts/ui-icons.py — do not edit. Phosphor {PHOSPHOR} (MIT), 256-unit viewBox. */\n")
        f.write("const PH = " + json.dumps(ui, separators=(",", ":")) + ";\n")
    with open(OUT_RS, "w") as f:
        f.write(f"// Generated by scripts/ui-icons.py — do not edit.\n"
                f"// Brand logos from Simple Icons {SIMPLE_ICONS} (CC0; redis from {PINNED['redis'][0]}; confluent from confluent.io); the trademarks belong to their owners.\n"
                f"// 24-unit viewBox, coordinates rounded to one decimal place.\n\n")
        f.write("/// (slug, brand colour, SVG path), sorted by slug.\n")
        f.write("pub static BRANDS: &[(&str, &str, &str)] = &[\n")
        for slug in sorted(brands):
            f.write(f'    ("{slug}", "{brands[slug][0]}", "{brands[slug][1]}"),\n')
        f.write("];\n\n/// (image repository, brand slug), sorted by repository.\n")
        f.write("pub static IMAGES: &[(&str, &str)] = &[\n")
        for image in sorted(images):
            f.write(f'    ("{image}", "{images[image]}"),\n')
        f.write("];\n")
    kb = lambda p: os.path.getsize(p) // 1024
    print(f"{len(brands)} logos for {len(images)} images -> brands.rs ({kb(OUT_RS)} KB in the binary); "
          f"{len(ui)} UI icons -> icons.js ({os.path.getsize(OUT_JS)} B in the page)")
    if too_big:
        print("too detailed, uses the stack icon:", ", ".join(f"{k} ({v} B)" for k, v in too_big.items()))
    if missing:
        print("not in Simple Icons (falls back to the stack icon):", ", ".join(missing))


main()
