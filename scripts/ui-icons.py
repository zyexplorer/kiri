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
PHOSPHOR_ICONS = {"stack": "stack", "cube": "cube", "database": "database", "network": "share-network", "gear": "gear-six", "textaa": "text-aa", "note": "note", "notepad": "notepad", "notepencil": "note-pencil", "pencil": "pencil-simple", "info": "info", "globe": "globe-simple", "chatdots": "chat-circle-dots", "sparkle": "sparkle", "wrench": "wrench", "shieldcheck": "shield-check", "trendup": "trend-up", "dot": "dot-outline", "hammer": "hammer", "checkcircle": "check-circle", "xcircle": "x-circle", "lightning": "lightning", "warning": "warning", "clock": "clock", "calendar": "calendar-blank", "steps": "stack-simple", "expand": "arrows-out-simple", "collapse": "arrows-in-simple", "minus": "minus-circle", "copy": "copy", "check": "check", "pods": "hexagon", "services": "plugs-connected", "sun": "sun", "moon": "moon", "link": "link-simple", "github": "github-logo", "sliders": "sliders", "play": "play", "open": "arrow-square-out", "pull": "download-simple", "trash": "trash", "logs": "article", "stop": "stop", "restart": "arrow-clockwise", "bug": "bug", "folder": "folder-open", "send": "paper-plane-tilt", "caret": "caret-right", "eye": "eye", "eyeoff": "eye-slash", "lock": "lock", "regen": "arrows-clockwise", "pause": "pause", "sidebar": "sidebar-simple", "broom": "broom", "updown": "arrows-down-up", "warncircle": "warning-circle", "confetti": "confetti", "starfill": "star-fill", "list": "list", "squares": "squares-four"}

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
                f"// Brand logos from Simple Icons {SIMPLE_ICONS} (CC0; redis from {PINNED['redis'][0]}); the trademarks belong to their owners.\n"
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
