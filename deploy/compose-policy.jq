# The policy of a rendered, merged compose (docs/design/deploy-config.md §1.6), in one place for
# render.sh, validate-compose.sh, preflight.sh, and verify-attestation.sh. Input: the compose as
# `docker compose config --format json` prints it. `violations($variant; $project)` is the list of
# broken rules, empty when the compose passes. $variant is `service`, `restore-check` (the topup
# CVM, deploy/RESTORE.md), `template` (the Phala Cloud template, deploy/compose.template.yaml), or
# `product` (the reference product); $project is `dstack` on a CVM. `allowed_envs_violations` checks
# the names a CVM's env may set against the compose.

def env_map:
    if type == "array" then map(capture("^(?<key>[^=]+)=(?<value>.*)$")) | from_entries
    elif . == null then {}
    else . end;

def check(ok; message): if ok then empty else message end;

def published: [.services | to_entries[] | select((.value.ports // []) | length > 0)
    | {service: .key, ports: .value.ports}];

# Only `$service` publishes, exactly `$port` → `$target` over TCP.
def only_published($service; $target; $port):
    published == [{service: $service, ports: [{mode: "ingress", target: $target,
        published: $port, protocol: "tcp"}]}];

def host_name: test("^[a-z0-9]([a-z0-9-]*[a-z0-9])?(\\.[a-z0-9]([a-z0-9-]*[a-z0-9])?)+$");

# The login of a `postgres://USER@postgres:5432/topup` URL.
def database_user: capture("^postgres://(?<user>[a-z_]+)@postgres:5432/topup$").user // "";

def volume_sources($service): [.services[$service].volumes[]? | .source];

# The content of the config a service mounts at `$target`.
def mounted_config($service; $target):
    . as $root
    | [.services[$service].configs[]? | select(.target == $target) | .source]
    | if length == 1 then $root.configs[.[0]].content // "" else "" end;

# The top-level `public_origin` of a topup.yaml, written as YAML or as JSON (`topup config show`).
def config_origin:
    (fromjson? | .public_origin? | strings)
    // ([split("\n")[] | capture("^public_origin:[ \\t]*[\"']?(?<origin>[^\"' \\t#]+)")] | first.origin)
    // "";

# The Phala Cloud template's per-deployment values, which its deploy form (and, for the origin,
# Phala Cloud's pre-launch script) puts in the CVM's env, so the attestation does not cover them
# (deploy/compose.template.yaml): WAL-G's backup location, which the postgres-walg entrypoint
# checks at startup, and the origin's host and the admin public key, which topup parses at startup
# from the variables its command names. Each may only be the whole value of its own key.
def template_runtime_setting($service; $key; $name):
    $name == $key and (
        (($service == "postgres" or $service == "backup")
            and ($key == "WALG_S3_PREFIX" or $key == "AWS_ENDPOINT" or $key == "AWS_REGION"))
        or ($service == "topup" and ($key == "DSTACK_APP_DOMAIN" or $key == "TOPUP_ADMIN_PUBLIC_KEY")));

# May the sealed secret `$name` fill the environment key `$key` of `$service`?
def secret_allowed($variant; $service; $key; $name):
    if $variant == "product" then
        $name == "PRODUCT_API_KEY" and $service == "product" and $key == $name
    else
        ($name == "SENTRY_DSN" and $service == "topup" and $key == $name)
        or (($name | test("^TOPUP_RPC_[A-Z0-9_]+_KEY$")) and $key == $name
            and ($service == "topup" or $service == "restore-check"))
        or (if $variant == "service" or $variant == "template" then
                ($name == "AWS_ACCESS_KEY_ID" or $name == "AWS_SECRET_ACCESS_KEY")
                and ($service == "postgres" or $service == "backup") and $key == $name
            else
                ($key == "AWS_ACCESS_KEY_ID" or $key == "AWS_SECRET_ACCESS_KEY")
                and $service == "postgres" and $name == "RESTORE_\($key)"
            end)
        or ($variant == "template" and template_runtime_setting($service; $key; $name))
    end;

# Every string of the compose that Compose would interpolate (a `$` left once `$$` is removed) must
# be exactly one allowed `${NAME:-}` environment value: a sealed value can fill nothing else.
def secret_violations($variant):
    . as $root
    | [paths(type == "string") as $path
        | ($root | getpath($path)) as $value
        | select($value | gsub("\\$\\$"; "") | contains("$"))
        | ($value | capture("^\\$\\{(?<name>[A-Z_][A-Z0-9_]*):-\\}$").name // null) as $name
        | if (($path | length) == 4 and $path[0] == "services" and $path[2] == "environment"
                and $name != null and secret_allowed($variant; $path[1]; $path[3]; $name))
            then empty
            else "a sealed value may not fill \($path | map(tostring) | join("."))"
          end];

# The compose's sealed names: its `${NAME:-}` environment values, the only references
# secret_violations allows.
def sealed_names:
    [.services[] | .environment | env_map | .[]
        | strings | capture("^\\$\\{(?<name>[A-Z_][A-Z0-9_]*):-\\}$").name] | unique;

# `$allowed`, the app-compose's allowed_envs (the names of the env file the Phala Cloud CLI sends),
# may name only sealed names of the compose, so the env can fill nothing the compose does not
# reference; the template's DSTACK_APP_DOMAIN is none, since the pre-launch script exports it. A
# sealed name it leaves out is unset in the CVM, as an empty one is: a required secret is enforced
# where it is used, at every start (deploy/scripts/postgres-walg-entrypoint.sh refuses S3 storage
# without both credentials, in PostgreSQL and the backup job).
def allowed_envs_violations($variant; $allowed):
    (sealed_names - if $variant == "template" then ["DSTACK_APP_DOMAIN"] else [] end) as $sealed
    | if ($allowed | type) == "array" and ($allowed | all(type == "string")) then
        [$allowed[] | select(. as $name | $sealed | index($name) | not)
            | "the env may set only the compose's sealed names, not \(.)"]
      else ["allowed_envs must be a list of names"] end;

def common_violations($variant; $project):
    . as $root
    | [ check(([.services[].image] | all(test("^[^@]+@sha256:[0-9a-f]{64}$") and (test("@sha256:0{64}$") | not)));
            "every image must be a nonzero repository@sha256 digest"),
      check(.name == $project; "the project must be \($project)"),
      check([.volumes // {} | to_entries[] | .value.name == "\($project)_\(.key)" and (.value.external | not)]
              | all; "every volume must be the project's own, named \($project)_<volume>"),
      check([.services[] | .build, .env_file, .extends, .profiles | select(. != null)] == [];
            "no service may build, read an env_file, extend, or carry a profile"),
      check([.configs // {} | to_entries[] | (.value.file == null) and (.value.content | type == "string")
              and (.key | test("^[a-z0-9_]+_[0-9a-f]{12}$"))] | all;
            "every config must be inline content named after its digest"),
      check([.services[] | select(.read_only == true) | .configs[]?
              | select($root.configs[.source].content != null)] == [];
            "inline config consumers must use a writable root filesystem for Compose injection"),
      check([.services[] | .environment | env_map | to_entries[]
              | select((.key | test("PASSWORD$")) or ((.value // "") | test("postgres(ql)?://[^/@]*:[^/@]*@")))]
              == []; "no service environment may carry a password"),
      check([.services | to_entries[] | .key as $service | .value.volumes[]?
              | select(.type == "bind") | select(.source != "/var/run/dstack.sock")] == [];
            "only the dstack socket may be bind-mounted")
    ] + secret_violations($variant)
      + (if $variant == "product" then [] elif $variant == "restore-check" then ["topup"] else ["topup", "smokescreen", "heartbeat"] end | map(
        . as $service
        | $root | [
          (if $service == "topup" then
            check(.services[$service].user == "999:999"; "topup must run as uid/gid 999")
           else
            check(.services[$service].read_only == true; "\($service) must use a read-only root filesystem")
           end),
          check((.services[$service].tmpfs // []) | length > 0; "\($service) must declare tmpfs"),
          check(.services[$service].cap_drop == ["ALL"]; "\($service) must drop all capabilities"),
          check((.services[$service].security_opt // []) | index("no-new-privileges:true") != null;
                "\($service) must disable privilege escalation"),
          check((.services[$service].mem_limit // "") | type == "string" and test("^[1-9][0-9]*[bkmgBKMG]$");
                "\($service) must set a positive finite memory limit"),
          check((.services[$service].pids_limit // 0) | type == "number" and . > 0;
                "\($service) must set a positive pids limit"),
          check((.services[$service].privileged // false) != true;
                "\($service) must not be privileged"),
          check((.services[$service].cap_add // []) == [];
                "\($service) must not add capabilities"),
          check((.services[$service].security_opt // []) == ["no-new-privileges:true"];
                "\($service) must use only no-new-privileges")
        ] | flatten
      ) | flatten);

# The derived credentials (deploy/README.md, "Database credentials"): each volume is the committed
# tmpfs, mounted by exactly these services, and written only by `keys`, which derives them.
def credential_mounters($variant):
    if $variant == "service" or $variant == "template" then
        {walg_key: ["backup", "keys", "postgres"], db_owner: ["backup", "keys", "migrate", "postgres"],
         db_app: ["heartbeat", "keys", "postgres", "topup"]}
    else
        {walg_key: ["keys", "postgres"], db_owner: ["keys", "migrate", "postgres", "restore-check"],
         db_app: ["keys", "postgres", "topup"]}
    end;

def credential_violations($variant):
    . as $root
    | credential_mounters($variant) | to_entries | map(
        .key as $volume | .value as $mounters
        | ($root.services | to_entries | map(select(any(.value.volumes[]?; .source == $volume)))) as $mounting
        | check($root.volumes[$volume] | (.external | not) and .driver == "local"
                and .driver_opts == {type: "tmpfs", device: "tmpfs", o: "uid=999,gid=999,mode=0700"};
              "\($volume) must be a tmpfs volume (uid=999,gid=999,mode=0700)"),
          check(($mounting | map(.key) | sort) == $mounters;
              "\($volume) must be mounted by exactly \($mounters | join(", "))"),
          check([$mounting[] | select(.key != "keys") | .value.volumes[] | select(.source == $volume)
                  | .read_only == true] | all;
              "only keys may mount \($volume) writable")
      ) | flatten;

# Smokescreen's command, with the deny ranges its defaults count as global (deploy/README.md,
# "Webhook egress"): exactly this, so no range can be dropped and no allowance added.
def smokescreen_command:
    ["smokescreen", "--listen-ip=0.0.0.0", "--listen-port=4750", "--timeout=10s",
     "--deny-range=0.0.0.0/8", "--deny-range=100.64.0.0/10", "--deny-range=169.254.0.0/16",
     "--deny-range=192.0.0.0/24", "--deny-range=198.18.0.0/15", "--deny-range=240.0.0.0/4"];

def topup_violations:
    (.services.topup.environment | env_map) as $topup
    | [ check(($topup.DATABASE_URL | database_user) == "topup_service"
                and $topup.PGPASSFILE == "/run/db-app/topup_service.pgpass";
              "topup must log in as topup_service with the application pgpass"),
        check((.services.migrate.environment | env_map | .DATABASE_URL | database_user) == "postgres";
              "migrate must log in as the database owner"),
        check(mounted_config("topup"; "/etc/topup/topup.yaml") != "";
              "topup must mount its topup.yaml at /etc/topup/topup.yaml"),
        check([.services.topup.configs[]? | (.uid // "0") == "0" and (.gid // "0") == "0"
                and (.mode // 292) == 292] | all;
              "topup configs must be root-owned with mode 0444")
      ];

# The live instance, the service or the template variant: topup serves with its webhooks through
# smokescreen, and PostgreSQL archives. The template's topup takes its origin and admin key from
# the variables its extra arguments name.
def serve_command: ["topup", "run", "--config", "/etc/topup/topup.yaml",
    "--webhook-proxy", "http://smokescreen:4750"];
def live_violations($command):
    [ check(.services.topup.command == $command;
              "topup must run the service with its webhooks through smokescreen"),
        check(.services.smokescreen.image == .services.topup.image
                and .services.smokescreen.command == smokescreen_command
                and .services.smokescreen.entrypoint == null
                and ((.services.smokescreen.ports // []) == []);
              "smokescreen must run its exact deny list from the service image, publishing nothing"),
        check([.services.postgres, .services.backup | .environment | env_map | .TOPUP_RESTORE_FROM_BACKUP // "off"]
                == ["off", "off"]; "the service must archive: TOPUP_RESTORE_FROM_BACKUP must not be on")
      ] + topup_violations + credential_violations("service");

# Capacity collector reads database storage and writes only its observability sample.
def capacity_violations:
    [check(.services.capacity.image == .services.postgres.image
        and .services.capacity.user == "999:999"
        and .services.capacity.entrypoint == ["bash", "/etc/topup/capacity.sh"];
        "capacity must use the PostgreSQL image and read-only probe entrypoint"),
     check((volume_sources("capacity") | sort) == ["observability", "pgdata"]
        and ([.services.capacity.volumes[] | select(.source == "pgdata") | .read_only] == [true]);
        "capacity may mount only read-only pgdata and observability"),
     check(.services.capacity.cap_drop == ["ALL"]
        and .services.capacity.security_opt == ["no-new-privileges:true"]
        and (.services.capacity.mem_limit != null) and (.services.capacity.pids_limit > 0);
        "capacity must have finite resource limits and no privileges")];

def service_violations:
    (.services["dstack-ingress"].environment | env_map) as $ingress
    | (mounted_config("topup"; "/etc/topup/topup.yaml") | config_origin) as $origin
    | [ check((.services | keys) == ["backup", "capacity", "dstack-ingress", "heartbeat", "keys", "migrate",
                "postgres", "smokescreen", "topup"];
              "the service runs exactly keys, postgres, migrate, topup, dstack-ingress, smokescreen, heartbeat, backup, and capacity"),
        check(only_published("dstack-ingress"; 443; "443");
              "only dstack-ingress may publish a port, 443"),
        check($ingress.TIMEOUT_CONNECT == "5s" and $ingress.TIMEOUT_CLIENT == "30s"
                and $ingress.TIMEOUT_SERVER == "30s" and $ingress.CHALLENGE_TYPE == "tls-alpn-01" and $ingress.TARGET_ENDPOINT == "topup:8080"
                and ($ingress.GATEWAY_DOMAIN // "" | host_name) and ($ingress.DOMAIN // "" | host_name)
                and $origin == "https://\($ingress.DOMAIN)";
              "dstack-ingress must serve the host of topup's public_origin with tls-alpn-01, forwarding to topup:8080, through a gateway host"),
        check([.services | to_entries[] | select(.value.volumes[]?.source == "/var/run/dstack.sock") | .key]
                | unique == ["dstack-ingress", "keys", "topup"];
              "only keys, topup, and dstack-ingress may mount the dstack socket")
      ] + live_violations(serve_command) + capacity_violations;

# The template: the service without dstack-ingress, topup published on 80 for the Phala Cloud
# gateway, which serves it as https://<app-id>.<gateway-domain>, the origin DSTACK_APP_DOMAIN names.
def template_violations:
    [ check((.services | keys) == ["backup", "capacity", "heartbeat", "keys", "migrate", "postgres", "smokescreen",
                "topup"];
              "the template runs exactly keys, postgres, migrate, topup, smokescreen, heartbeat, backup, and capacity"),
        check(only_published("topup"; 8080; "80"); "only topup may publish a port, 80"),
        check([.services | to_entries[] | select(.value.volumes[]?.source == "/var/run/dstack.sock") | .key]
                | unique == ["keys", "topup"];
              "only keys and topup may mount the dstack socket")
      ] + live_violations(serve_command + ["--public-origin-host-env", "DSTACK_APP_DOMAIN",
            "--admin-public-key-env", "TOPUP_ADMIN_PUBLIC_KEY"]) + capacity_violations;

def restore_check_violations:
    (.services.postgres.environment | env_map) as $postgres
    | (.services.topup.command // []) as $command
    | (.services["restore-check"].environment | env_map) as $check
    | [ check((.services | keys) == ["keys", "migrate", "postgres", "restore-check", "topup"];
              "restore-check runs exactly keys, postgres, migrate, topup, and restore-check"),
        check(only_published("topup"; 8080; "8081"); "only topup may publish a port, 8081"),
        check($command[0:8] == ["topup", "run", "--config", "/etc/topup/topup.yaml", "--read-only",
                "--restore-report", "/run/topup-observability/restore-check.json", "--public-origin"]
                and ($command | length) == 9 and ($command[8] | test("^https://[^/]+$"));
              "topup must serve read-only under the restore instance's own https origin"),
        check($postgres.TOPUP_RESTORE_FROM_BACKUP == "on" and .services.postgres.command == null
                and .services.postgres.entrypoint == null;
              "postgres must restore with archiving off (TOPUP_RESTORE_FROM_BACKUP=on, the image's own entrypoint and command)"),
        check($postgres.AWS_ACCESS_KEY_ID == "${RESTORE_AWS_ACCESS_KEY_ID:-}"
                and $postgres.AWS_SECRET_ACCESS_KEY == "${RESTORE_AWS_SECRET_ACCESS_KEY:-}";
              "postgres must read the restore instance's own storage credentials"),
        check(.services["restore-check"].entrypoint[0:4] == ["topup", "restore-check", "--config", "/etc/topup/topup.yaml"]
                and ($check.DATABASE_URL | database_user) == "postgres"
                and (volume_sources("restore-check") | index("/var/run/dstack.sock")) != null;
              "restore-check must check the restored database as its owner, with the dstack socket"),
        check([.services | to_entries[] | select(.value.volumes[]?.source == "/var/run/dstack.sock") | .key]
                | unique == ["keys", "restore-check", "topup"];
              "only keys, topup, and restore-check may mount the dstack socket")
      ] + topup_violations + credential_violations("restore-check");

def product_violations:
    (.services["dstack-ingress"].environment | env_map) as $ingress
    | (mounted_config("product"; "/etc/product/config.json") | fromjson? // {}) as $config
    | [ check((.services | keys) == ["dstack-ingress", "product"];
              "the product CVM runs exactly product and dstack-ingress"),
        check(only_published("dstack-ingress"; 443; "443"); "only dstack-ingress may publish a port, 443"),
        check($ingress.TIMEOUT_CONNECT == "5s" and $ingress.TIMEOUT_CLIENT == "30s"
                and $ingress.TIMEOUT_SERVER == "30s" and $ingress.CHALLENGE_TYPE == "tls-alpn-01" and $ingress.TARGET_ENDPOINT == "product:8089"
                and ($ingress.GATEWAY_DOMAIN // "" | host_name) and ($ingress.DOMAIN // "" | host_name)
                and $config.public_url == "https://\($ingress.DOMAIN)";
              "dstack-ingress must serve the host of the product's public_url with tls-alpn-01, forwarding to product:8089"),
        check([.services | to_entries[] | select(.value.volumes[]?.source == "/var/run/dstack.sock") | .key]
                == ["dstack-ingress"]; "only dstack-ingress may mount the dstack socket")
      ];

def violations($variant; $project):
    common_violations($variant; $project)
    + if $variant == "service" then service_violations
      elif $variant == "restore-check" then restore_check_violations
      elif $variant == "template" then template_violations
      elif $variant == "product" then product_violations
      else ["unknown variant \($variant)"] end;
