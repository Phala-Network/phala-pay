# The policy of a rendered, merged compose (docs/design/deploy-config.md §1.6), in one place for
# render.sh, validate-compose.sh, preflight.sh, and verify-attestation.sh. Input: the compose as
# `docker compose config --format json` prints it. `violations($variant; $project)` is the list of
# broken rules, empty when the compose passes. $variant is `service`, `restore-check` (the topup
# CVM, deploy/RESTORE.md), `template` (the Phala Cloud template, deploy/compose.template.yaml), or
# `product` (the reference product), `export`, or `migration-import`; $project is `dstack` on a CVM. `allowed_envs_violations` checks
# the names a CVM's env may set against the compose.

def env_map:
    if type == "array" then map(capture("^(?<key>[^=]+)=(?<value>.*)$")) | from_entries
    elif . == null then {}
    else . end;

def check(ok; message): if ok then empty else message end;

# Explicit for every variant, including those without an ingress. `instances add` copies the
# service's app-compose, so restore-check inherits its PROXY-on policy for 443. Nothing listens
# on 443 there; only plain HTTP on 8081 is published. Template and product keep PROXY off.
def port_policy($variant):
    if (["service", "restore-check", "template", "product", "export", "migration-import"] | index($variant)) != null then
        {ports: [{port: 443, pp: ($variant == "service" or $variant == "restore-check" or $variant == "export" or $variant == "migration-import")}], restrict_mode: false}
    else error("unknown variant \($variant)") end;

def port_policy_violations($variant; $app):
    [check($app.port_policy == port_policy($variant);
        "the attested port_policy must match the \($variant) policy (PROXY \(if port_policy($variant).ports[0].pp then "on" else "off" end), restrict_mode false)")];

def ingress_violations($proxy):
    [check([.services["dstack-ingress"].environment | env_map
        | .ACCEPT_PROXY_PROTOCOL, .SEND_PROXY_PROTOCOL]
        | all(if $proxy then . == "true" else . == null or . == "false" end);
        "dstack-ingress must set ACCEPT_PROXY_PROTOCOL and SEND_PROXY_PROTOCOL to \(if $proxy then "true" else "false or omit them" end)")];

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
        or (($name == "AWS_ACCESS_KEY_ID" or $name == "AWS_SECRET_ACCESS_KEY") and $key == $name
            and ($service == "postgres" or ($service == "backup" and $variant != "restore-check")))
        or (($variant == "export" and $service == "export" or $variant == "migration-import" and $service == "import")
            and ($name == "AWS_ACCESS_KEY_ID" or $name == "AWS_SECRET_ACCESS_KEY") and $key == $name)
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

# `$allowed`, the app-compose's allowed_envs (inherited from the source for `instances add`),
# may name only sealed names of the compose, so the env can fill nothing the compose does not
# reference; the template's DSTACK_APP_DOMAIN is none, since the pre-launch script exports it. A
# sealed name it leaves out is unset in the CVM, as an empty one is: a required secret is enforced
# where it is used, at every start (deploy/scripts/postgres-walg-entrypoint.sh refuses S3 storage
# without both credentials, in PostgreSQL and the backup job).
def allowed_envs_violations($variant; $allowed):
    # Export retains the stopped service's allowlist, but cannot interpolate unused names.
    # Other variants still accept only names actually referenced by their compose.
    (sealed_names - if $variant == "template" then ["DSTACK_APP_DOMAIN"] else [] end) as $sealed
    | if ($allowed | type) == "array" and ($allowed | all(type == "string")) then
        [$allowed[] | select((. as $name | $sealed | index($name)) == null)
            | select($variant != "export" or (test("^(SENTRY_DSN|TOPUP_RPC_[A-Z0-9_]+_KEY)$") | not))
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
              | select((.key | test("PASSWORD$")) or (.value
                  | if type == "string" then test("postgres(ql)?://[^/@]*:[^/@]*@") else false end))]
              == []; "no service environment may carry a password"),
      check([.services | to_entries[] | .key as $service | .value.volumes[]?
              | select(.type == "bind") | select(.source != "/var/run/dstack.sock")] == [];
            "only the dstack socket may be bind-mounted")
    ] + secret_violations($variant)
      + (if $variant == "product" or $variant == "export" then [] elif $variant == "restore-check" then ["topup"] else ["topup", "smokescreen", "heartbeat"] end | map(
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
    if $variant == "export" then
        {walg_key: ["backup", "keys", "postgres"], db_owner: ["backup", "dump", "export", "keys", "postgres", "snapshot"],
         db_app: ["keys", "postgres"]}
    elif $variant == "service" or $variant == "template" then
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
                and $ingress.TIMEOUT_SERVER == "30s" and $ingress.CHALLENGE_TYPE == "tls-alpn-01" and $ingress.TARGET_ENDPOINT == "topup:8082"
                and ($ingress.GATEWAY_DOMAIN // "" | host_name) and ($ingress.DOMAIN // "" | host_name)
                and $origin == "https://\($ingress.DOMAIN)";
              "dstack-ingress must serve the host of topup's public_origin with tls-alpn-01, forwarding to topup:8082, through a gateway host"),
        check([.services | to_entries[] | select(.value.volumes[]?.source == "/var/run/dstack.sock") | .key]
                | unique == ["dstack-ingress", "keys", "topup"];
              "only keys, topup, and dstack-ingress may mount the dstack socket")
      ] + ingress_violations(true)
      + live_violations(serve_command + ["--proxy-protocol-bind", "0.0.0.0:8082"]) + capacity_violations;

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
        check($postgres.AWS_ACCESS_KEY_ID == "${AWS_ACCESS_KEY_ID:-}"
                and $postgres.AWS_SECRET_ACCESS_KEY == "${AWS_SECRET_ACCESS_KEY:-}";
              "postgres must read the app's allowed AWS_* names from the restore instance's read-only env"),
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
    | [ check((.services | keys) == ["dstack-ingress", "guest-info", "product"];
              "the product CVM runs exactly product, dstack-ingress, and guest-info"),
        check(only_published("dstack-ingress"; 443; "443"); "only dstack-ingress may publish a port, 443"),
        check($ingress.TIMEOUT_CONNECT == "5s" and $ingress.TIMEOUT_CLIENT == "30s"
                and $ingress.TIMEOUT_SERVER == "30s" and $ingress.CHALLENGE_TYPE == "tls-alpn-01" and $ingress.TARGET_ENDPOINT == "product:8089"
                and ($ingress.GATEWAY_DOMAIN // "" | host_name) and ($ingress.DOMAIN // "" | host_name)
                and $config.public_url == "https://\($ingress.DOMAIN)";
              "dstack-ingress must serve the host of the product's public_url with tls-alpn-01, forwarding to product:8089"),
        check([.services | to_entries[] | select(.value.volumes[]?.source == "/var/run/dstack.sock") | .key]
                == ["dstack-ingress", "guest-info"]; "only dstack-ingress and guest-info may mount the dstack socket; product must never mount it"),
        check(.services["guest-info"] as $info
            | $info.command == ["topup", "guest-info", "serve"] and ($info.entrypoint // null) == null
            and ($info.environment // {} | length) == 0 and ($info.configs // [] | length) == 0
            and ($info.secrets // [] | length) == 0 and ($info.ports // [] | length) == 0
            and $info.read_only == true and $info.cap_drop == ["ALL"] and ($info.cap_add // []) == []
            and ($info.privileged // false) == false and $info.security_opt == ["no-new-privileges:true"]
            and $info.mem_limit == "128m" and $info.pids_limit == 32
            and $info.networks == {guest_info:null} and ($info.network_mode // null) == null
            and ($info.volumes | length) == 1;
            "guest-info must run only the Info server without credentials, extra mounts, ports or privileges"),
        check(.networks.guest_info.internal == true
            and (.services.product.networks | keys) == ["default", "guest_info"]
            and ([.services | to_entries[] | select(.value.networks.guest_info != null or (.value.networks | has("guest_info"))) | .key] == ["guest-info", "product"]);
            "only product and guest-info may share the internal Info network")
      ] + ingress_violations(false);

# One-time migration variants. Keep the split postgres/crypto trust boundary explicit.
def completed($service; $dependency):
    .services[$service].depends_on[$dependency].condition == "service_completed_successfully";

def migration_volume_violations($mounters):
    [check(.volumes.migration.driver == "local"
        and .volumes.migration.driver_opts == {type: "tmpfs", device: "tmpfs", o: "uid=999,gid=999,mode=0700,size=4g"};
        "migration plaintext must use the private bounded tmpfs"),
     check([.services | to_entries[] | select(.value.volumes[]?.source == "migration") | .key] == $mounters;
        "only migration jobs may mount the plaintext tmpfs"),
     check(.services["migration-holder"] as $holder
        | $holder.image == .services.postgres.image and $holder.user == "999:999"
        and $holder.entrypoint == ["sleep", "infinity"] and $holder.command == []
        and $holder.network_mode == "none" and ($holder.environment // {}) == {}
        and ($holder.ports // []) == [] and ($holder.configs // []) == []
        and volume_sources("migration-holder") == ["migration"]
        and $holder.read_only == true and $holder.cap_drop == ["ALL"]
        and $holder.security_opt == ["no-new-privileges:true"] and $holder.mem_limit != null
        and $holder.pids_limit == 8 and $holder.restart == "unless-stopped";
        "the plaintext tmpfs holder must use only the pinned image, no network or credentials"),
     check([.services | to_entries[] | select(.key != "migration-holder")
        | select(.value.volumes[]?.source == "migration")
        | .value.depends_on["migration-holder"].condition == "service_started"] | all;
        "the plaintext holder must start before every transfer job")];

def migration_crypto_violations($service; $mode; $verify):
    .services[$service] as $job
    | [check($job.image == .services.keys.image and $job.user == "999:999"
        and $job.entrypoint == null and $job.restart == "no"
        and $job.command == (["topup", "migrate", $mode, "--config", "/etc/topup/migration.json"]
            + if $verify then ["--verify"] else [] end);
        "migration crypto must run the exact topup job as uid 999"),
       check(($job.environment | env_map) as $env
        | ($env.DATABASE_URL | database_user) == "postgres" and $env.TOKIO_WORKER_THREADS == "2" and $env.PGPASSFILE == "/run/db-owner/postgres.pgpass";
        "migration crypto must use the owner pgpass"),
       check((mounted_config($service; "/etc/topup/migration.json") | fromjson? // {})
          | if $mode == "export" then
                keys == ["migration_export"] and (.migration_export | keys == ["object", "recipient"]
                    and (.recipient | test("^age1[023456789acdefghjklmnpqrstuvwxyz]{58}$")) and (.object | startswith("s3://")))
            else keys == ["migration_import"] and (.migration_import | keys == ["export_public_key", "object", "source_app_id"]
                    and (.export_public_key | test("^[0-9a-f]{64}$")) and (.source_app_id | test("^[0-9a-f]{40,64}$"))
                    and (.object | startswith("s3://"))) end;
        "migration pins must be inline attested configuration"),
       check((volume_sources($service) | sort) == ["/var/run/dstack.sock", "db_owner", "migration"]
        and [$job.volumes[] | select(.source == "db_owner") | .read_only] == [true];
        "migration crypto may mount only its socket, read-only owner credentials and plaintext tmpfs"),
       check($job.cap_drop == ["ALL"] and $job.security_opt == ["no-new-privileges:true"]
        and $job.mem_limit != null and $job.pids_limit > 0;
        "migration crypto must have resource and privilege limits")];

def migration_pg_violations($service; $command):
    .services[$service] as $job
    | [check($job.image == .services.postgres.image and $job.user == "999:999"
        and $job.entrypoint == ["bash", "-ceu"] and $job.command == [$command] and $job.restart == "no";
        "migration PostgreSQL tools must run the exact archive job from the pinned postgres image"),
       check($job.environment == ({PGHOST: "postgres", PGDATABASE: "topup", PGUSER: "postgres", PGPASSFILE: "/run/db-owner/postgres.pgpass"}
            + if $service == "restore" then {PGOPTIONS: "-c log_min_messages=panic -c log_min_error_statement=panic -c log_statement=none"} else {} end)
        and (volume_sources($service) | sort) == ["db_owner", "migration"]
        and [$job.volumes[] | select(.source == "db_owner") | .read_only] == [true];
        "migration PostgreSQL tools may mount only owner credentials and the plaintext tmpfs")];

def export_violations:
    [check((.services | keys) == ["backup", "dump", "export", "keys", "migration-holder", "postgres", "snapshot"] and published == [];
        "export must fence topup, ingress, heartbeat and migrations; no ports"),
     check(.services.backup.entrypoint == ["bash", "-ceu"] and .services.backup.restart == "no"
        and .services.backup.command == ["psql -X -v ON_ERROR_STOP=1 -c \"SELECT pg_switch_wal()\"; wal-g backup-push /var/lib/postgresql/data"]
        and completed("snapshot"; "backup") and .services.dump.depends_on.snapshot.condition == "service_healthy"
        and completed("export"; "dump") and completed("export"; "snapshot");
        "final backup must precede dump and encryption"),
     check([.services | to_entries[] | select(.value.volumes[]?.source == "/var/run/dstack.sock") | .key]
        == ["export", "keys"]; "only export and keys may mount the dstack socket")]
    + credential_violations("export") + migration_volume_violations(["dump", "export", "migration-holder", "snapshot"])
    + migration_crypto_violations("export"; "export"; false)
    + [check(.services.snapshot.image == .services.keys.image and .services.snapshot.user == "999:999"
        and .services.snapshot.command == ["topup", "migrate", "export", "--config", "/etc/topup/migration.json", "--snapshot"]
        and .services.snapshot.healthcheck.test == ["CMD", "topup", "migrate", "export", "--config", "/etc/topup/migration.json", "--snapshot-check"]
        and .services.snapshot.entrypoint == null and .services.snapshot.restart == "no"
        and (volume_sources("snapshot") | sort) == ["db_owner", "migration"]
        and (.services.snapshot.environment | env_map) == {DATABASE_URL:"postgres://postgres@postgres:5432/topup", TOKIO_WORKER_THREADS:"2", PGPASSFILE:"/run/db-owner/postgres.pgpass"}
        and mounted_config("snapshot"; "/etc/topup/migration.json") == mounted_config("export"; "/etc/topup/migration.json");
        "the snapshot job must hold the exact dump snapshot and publish its metadata")]
    + migration_pg_violations("dump"; "umask 077; pg_dump -Fc --snapshot=\"$$(cat /run/migration/snapshot.id)\" --exclude-schema=topup_migration --file=/run/migration/dump.pg 2>/dev/null || { rm -f /run/migration/dump.pg; echo \"migration dump failed (private tmpfs limit: 4 GiB; details suppressed)\" >&2; exit 1; }; touch /run/migration/dump.done");

def migration_import_violations:
    . as $original
    # Apply the stock service policy after projecting away only the declared import additions.
    | (del(.services["import", "restore", "import-verify", "migration-holder"], .volumes.migration,
           .services.migrate.depends_on["import-verify"], .services.backup.depends_on["import-verify"])
       | service_violations)
    + ($original | [
      check(completed("restore"; "import") and completed("import-verify"; "restore")
        and completed("migrate"; "import-verify") and completed("backup"; "import-verify")
        and completed("topup"; "migrate") and completed("heartbeat"; "migrate")
        and .services.import.depends_on.postgres.condition == "service_healthy";
        "import, transactional restore and verification must complete before migrate, topup, heartbeat and backup"),
      check([.services | to_entries[] | select(.value.volumes[]?.source == "/var/run/dstack.sock") | .key]
        == ["dstack-ingress", "import", "import-verify", "keys", "topup"];
        "only import jobs and service identity consumers may mount the dstack socket")]
      + migration_volume_violations(["import", "import-verify", "migration-holder", "restore"])
      + migration_crypto_violations("import"; "import"; false)
      + migration_crypto_violations("import-verify"; "import"; true)
      + migration_pg_violations("restore"; "trap \"rm -f /run/migration/dump.pg\" EXIT; marker=$$(psql -XAtqc \"SELECT to_regclass('topup_migration.imported') IS NOT NULL\"); if [ \"$$marker\" = t ]; then exit 0; fi; pg_restore --single-transaction --exit-on-error --dbname=topup /run/migration/dump.pg 2>/dev/null || { echo \"migration restore failed (details suppressed)\" >&2; exit 1; }"));

def violations($variant; $project):
    common_violations($variant; $project)
    + if $variant == "export" then export_violations
      elif $variant == "migration-import" then migration_import_violations
      elif $variant == "service" then service_violations
      elif $variant == "restore-check" then restore_check_violations
      elif $variant == "template" then template_violations
      elif $variant == "product" then product_violations
      else ["unknown variant \($variant)"] end;

# Onchain KMS adds explicit provider pins. Phala Cloud deployments retain their existing policy.
# $identity is the Base KmsAuth.kmsInfo().caPubkey (hex DER), read after checking its proxy pin.
def kms_violations($kind; $app; $identity):
    if $kind == "phala" then []
    elif $kind == "base" then
        [check($app.kms_enabled == true; "Base KMS requires kms_enabled"),
         check($app.key_provider == "kms"; "Base KMS requires key_provider kms"),
         check(($identity | type == "string" and test("^[0-9a-f]+$")) and
            (($app.key_provider_id // "" | ascii_downcase | ltrimstr("0x")) == $identity);
            "Base KMS key_provider_id must match KmsAuth.kmsInfo().caPubkey")]
    else ["kms must be phala or base"] end;


# Every Base non-render field, including the exact key set, is reviewed in the environment.
# Cloud cannot introduce even an unused key. Rendered compose/pre-launch bytes are checked
# separately by attested-compose.sh; all other values must match this baseline exactly.
def base_app_violations($app; $baseline):
    [check(($app | del(.docker_compose_file, .pre_launch_script)) ==
            ($baseline | del(.docker_compose_file, .pre_launch_script));
        "Base app-compose non-render fields/key set differ from the reviewed baseline"),
     check(($app | has("init_script") | not) or $app.init_script == [];
        "Base app-compose init_script must be absent or an empty array"),
     check($app.kms_enabled == true and $app.key_provider == "kms" and
        $app.local_key_provider_enabled == false and $app.no_instance_id == false and
        $app.public_logs == false and $app.public_sysinfo == false and
        $app.public_tcbinfo == true and $app.secure_time == true and
        $app.gateway_enabled == true and $app.tproxy_enabled == true and
        $app.storage_fs == "ext4" and $app.runner == "docker-compose" and
        ([2, 3, "2", "3"] | index($app.manifest_version)) != null and
        $app.features == ["kms", "tproxy-net"];
        "Base app-compose flags must match the supported secure configuration"),
     check(($app.allowed_envs | type == "array" and all(type == "string") and length == (unique | length));
        "Base allowed_envs must be a unique reviewed list")];
