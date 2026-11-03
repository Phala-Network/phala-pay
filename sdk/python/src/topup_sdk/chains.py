"""Explorer URLs, kept in parity with the JS SDK's viem chain registry by shared fixtures."""

import re

# Official explorers from viem/chains: mainnet, sepolia, base, baseSepolia.
_NETWORKS = {
    1: ("Ethereum", "https://etherscan.io"),
    11155111: ("Sepolia", "https://sepolia.etherscan.io"),
    8453: ("Base", "https://basescan.org"),
    84532: ("Base Sepolia", "https://sepolia.basescan.org"),
}


def network_name(chain_id: int) -> str:
    """Human network name, or ``Chain <id>`` for an unknown network."""
    network = _NETWORKS.get(chain_id)
    return network[0] if network else f"Chain {chain_id}"


def transaction_url(chain_id: int, tx_hash: str) -> str | None:
    """Explorer URL, or None for an unknown network or malformed transaction hash."""
    if re.fullmatch(r"0x[0-9a-fA-F]{64}", tx_hash) is None:
        return None
    network = _NETWORKS.get(chain_id)
    return f"{network[1]}/tx/{tx_hash}" if network else None


def address_url(chain_id: int, address: str) -> str | None:
    """Explorer URL, or None for an unknown network or malformed address."""
    if re.fullmatch(r"0x[0-9a-fA-F]{40}", address) is None:
        return None
    network = _NETWORKS.get(chain_id)
    return f"{network[1]}/address/{address}" if network else None
