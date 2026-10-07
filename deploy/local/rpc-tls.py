"""Write certificate-verified local RPC relays and rewrite a resolved test configuration."""
from __future__ import annotations

import argparse
import json
from pathlib import Path


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("config", type=Path)
    parser.add_argument("overlay", type=Path)
    parser.add_argument("--certificate", type=Path, required=True)
    parser.add_argument("--key", type=Path, required=True)
    parser.add_argument("--image", required=True)
    parser.add_argument("--chain", action="append", required=True, help="chain_id=http://upstream:port")
    args = parser.parse_args()
    upstreams = dict(item.split("=", 1) for item in args.chain)
    config = json.loads(args.config.read_text())
    services = {}
    relays = {}
    for chain in config["rpc"]:
        chain_id = str(chain["chain_id"])
        upstream = upstreams[chain_id]
        for role in ("read", "verify"):
            name = f"rpc-{chain_id}-{role}"
            host = f"{name}.rpc.test"
            chain[role]["url"] = f"https://{host}:8443/{{key}}"
            services[name] = {
                "image": args.image,
                "entrypoint": ["python3", "/etc/rpc-tls/proxy.py"],
                "command": ["--certificate", "/etc/rpc-tls/cert.pem", "--key", "/etc/rpc-tls/key.pem", "--upstream", upstream],
                "configs": [
                    {"source": "rpc_tls_proxy", "target": "/etc/rpc-tls/proxy.py"},
                    {"source": "rpc_tls_certificate", "target": "/etc/rpc-tls/cert.pem"},
                    {"source": "rpc_tls_key", "target": "/etc/rpc-tls/key.pem"},
                ],
                "networks": {"default": {"aliases": [host]}},
                "cap_drop": ["ALL"],
                "security_opt": ["no-new-privileges:true"],
                "restart": "no",
            }
            relays[name] = {"condition": "service_started"}
    for service in ("topup", "restore-check"):
        services[service] = {
            "environment": {
                "SSL_CERT_FILE": "/etc/rpc-tls/cert.pem",
                "TOPUP_RPC_ANKR_KEY": "${TOPUP_RPC_ANKR_KEY:-local-rpc-key}",
                "TOPUP_RPC_INFURA_KEY": "${TOPUP_RPC_INFURA_KEY:-local-rpc-key}",
            },
            "configs": [{"source": "rpc_tls_certificate", "target": "/etc/rpc-tls/cert.pem"}],
            "depends_on": relays,
        }
    overlay = {
        "services": services,
        "configs": {
            "rpc_tls_proxy": {"content": Path(__file__).with_name("tls_proxy.py").read_text()},
            "rpc_tls_certificate": {"content": args.certificate.read_text()},
            "rpc_tls_key": {"content": args.key.read_text()},
        },
    }
    args.overlay.write_text(json.dumps(overlay))
    args.config.write_text(json.dumps(config))


if __name__ == "__main__":
    main()
