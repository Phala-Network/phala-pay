"""Write disposable test overlays; the actual rendered migration jobs remain unchanged."""
from __future__ import annotations

import json
import sys
from pathlib import Path

root, temporary, topup_image, postgres_image, simulator_image, project = sys.argv[1:]
root = Path(root)
temporary = Path(temporary)
# The same isolated Garage fixtures used by every existing CVM/restore rehearsal.
base = json.loads((temporary / "s3-base.json").read_text())
python_image = base["services"]["s3-init"]["image"]
certificate = (temporary / "cert.pem").read_text()
ca = (temporary / "ca.pem").read_text()
key = (temporary / "key.pem").read_text()
proxy = (root / "deploy/local/tls_proxy.py").read_text()
source_network, destination_network = f"{project}-source", f"{project}-destination"
support = {
    **base,
    "services": {
        **base["services"],
        "dstack-simulator": {
            "image": simulator_image,
            "volumes": ["dstack_socket:/var/run"],
            "healthcheck": {"test": ["CMD", "test", "-S", "/var/run/dstack.sock"], "interval": "1s", "timeout": "2s", "retries": 60},
        },
        "s3-tls": {
            "image": python_image,
            "entrypoint": ["python3", "/etc/test-tls/proxy.py"],
            "command": ["--bind", "0.0.0.0", "--upstream", "http://s3:3900"],
            "configs": [
                {"source": name, "target": target}
                for name, target in [("tls_proxy", "/etc/test-tls/proxy.py"), ("tls_cert", "/etc/test-tls/cert.pem"), ("tls_key", "/etc/test-tls/key.pem")]
            ],
            "networks": ["default", "source", "destination"],
            "depends_on": {"s3": {"condition": "service_healthy"}},
        },
    },
    "configs": {**base["configs"], "tls_proxy": {"content": proxy}, "tls_cert": {"content": certificate}, "tls_key": {"content": key}},
    "volumes": {**base["volumes"], "dstack_socket": {}},
    "networks": {**base.get("networks", {}), "source": {"name": source_network}, "destination": {"name": destination_network}},
}
(temporary / "support.json").write_text(json.dumps(support))
for variant, network in [("source", source_network), ("destination", destination_network)]:
    services = {}
    names = ["keys", "postgres", "backup", "snapshot", "export"] if variant == "source" else ["keys", "postgres", "backup", "import", "import-verify"]
    for name in names:
        settings = {}
        if name in ["keys", "export", "import", "import-verify"]:
            # Replacement volumes are emitted below as a Compose !override list by the shell.
            settings["volumes"] = []
        if name in ["postgres", "backup", "export", "import"]:
            settings["environment"] = {"SSL_CERT_FILE": "/etc/test-tls/ca.pem"}
            settings["configs"] = [{"source": "tls_ca", "target": "/etc/test-tls/ca.pem"}]
        if name == "postgres":
            # The official entrypoint's temporary initialization server listens only on Unix.
            # Probe final TCP readiness so the actual one-shot jobs cannot race initialization.
            settings["environment"]["PGPASSFILE"] = "/run/db-owner/postgres.pgpass"
            settings["healthcheck"] = {"test": ["CMD-SHELL", "psql -h 127.0.0.1 -U postgres -d topup -XAtqc 'SELECT NOT pg_is_in_recovery()' | grep -qx t"], "interval": "1s", "timeout": "5s", "retries": 60}
        services[name] = settings
    overlay = {"services": services, "configs": {"tls_ca": {"content": ca}},
               "volumes": {"dstack_socket": {"external": True, "name": f"{project}-support_dstack_socket"}, "migration": {"driver_opts": {"o": "uid=999,gid=999,mode=0700,size=64m"}}},
               "networks": {"default": {"external": True, "name": network}}}
    (temporary / f"{variant}-tls.json").write_text(json.dumps(overlay))
    # The test changes only the host socket to the pinned simulator socket volume. The actual
    # dump/restore/crypto commands, dependencies, private tmpfs and holder are left as rendered.
    sockets = "services:\n"
    for name in ["keys", "export"] if variant == "source" else ["keys", "import", "import-verify"]:
        mounts = ["dstack_socket:/var/run", "walg_key:/run/wal-g", "db_owner:/run/db-owner", "db_app:/run/db-app"] if name == "keys" else ["dstack_socket:/var/run", "db_owner:/run/db-owner:ro", "migration:/run/migration"]
        sockets += f"  {name}:\n    volumes: !override\n" + "".join(f"      - {mount}\n" for mount in mounts)
    (temporary / f"{variant}-sockets.yaml").write_text(sockets)
images = {"phala-pay": topup_image, "postgres-walg": postgres_image}
(temporary / "images.json").write_text(json.dumps(images))
for variant in ["source", "destination"]:
    environment = temporary / f"{variant}-env"
    environment.mkdir()
    (environment / "topup.yaml").write_text((root / "deploy/environments/phala-network/staging/topup/topup.yaml").read_text())
    settings = {name: {"environment": {
        "WALG_S3_PREFIX": f"s3://topup-backups/{variant}", "AWS_ENDPOINT": "https://s3-tls:8443", "AWS_REGION": "us-east-1", "AWS_S3_FORCE_PATH_STYLE": "true"
    }} for name in ["postgres", "backup"]}
    settings["dstack-ingress"] = {"environment": {"DOMAIN": "pay-api-staging.phala.com"}}
    (environment / "compose.yaml").write_text(json.dumps({"services": settings}))
