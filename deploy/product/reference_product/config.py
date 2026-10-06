"""The product's configuration, shared by the product service and the deposit driver."""

from __future__ import annotations

import json
import os
import re
from dataclasses import dataclass, field
from pathlib import Path
from urllib.parse import urlsplit

from topup_sdk import TopupClient
from topup_sdk.addresses import to_checksum_address

from .transport import DeadlineTransport

# The deposit driver signs its account API requests with this key id (see `AccountApi`).
DRIVER_KEYID = "driver/v1"
EVM_ADDRESS = re.compile(r"0x[0-9a-fA-F]{40}")
ACCOUNT_ID = re.compile(r"acct_[0-9a-f]{32}")
# An origin as browsers serialize it in `Origin`: https with a lowercase host and no path; http
# only on the loopback host, for local runs.
WEB_ORIGIN = re.compile(
    r"(https://[a-z0-9]([a-z0-9.-]*[a-z0-9])?|http://(127\.0\.0\.1|localhost))(:[1-9][0-9]{0,4})?"
)
# A token's symbol (`PHA`) and a service asset code (`pha`).
SYMBOL = re.compile(r"[A-Z0-9]{1,11}")
ASSET_CODE = re.compile(r"[a-z0-9]{1,11}")
# A path segment that looks like an API key, as deploy/preflight-phala.sh's `embeds_key` reads it:
# 20 or more URL-safe characters with a digit.
KEY_LIKE = re.compile(r"(?=[A-Za-z0-9_-]*[0-9])[A-Za-z0-9_-]{20,}")


class MissingProductKeyError(Exception):
    """The product's API key is not configured (in a CVM: not sealed yet)."""


def checksum_address(value: object, name: str) -> str:
    """`value` as an EIP-55 address: a single-case address is normalised, a mixed-case one must
    be its checksum."""
    if not isinstance(value, str) or not EVM_ADDRESS.fullmatch(value):
        raise ValueError(f"{name} must be a 0x-prefixed 20-byte address")
    checksummed = to_checksum_address(value)
    digits = value[2:]
    if value != checksummed and digits not in (digits.lower(), digits.upper()):
        raise ValueError(f"{name} does not match its EIP-55 checksum")
    return checksummed


def keyless_rpc_url(value: object, name: str) -> str:
    """A public RPC URL the attested config may publish: https (http only on the loopback host or
    a single-label host, a local chain), and no user info, query, or key-like path segment."""
    if not isinstance(value, str):
        raise ValueError(f"{name} must be a URL")
    parts = urlsplit(value)
    host = parts.hostname or ""
    local = host in ("127.0.0.1", "localhost") or (host != "" and "." not in host)
    if not host or not (parts.scheme == "https" or (parts.scheme == "http" and local)):
        raise ValueError(f"{name} must be an https URL")
    if (
        parts.username is not None
        or parts.query
        or parts.fragment
        or any(KEY_LIKE.fullmatch(segment) for segment in parts.path.split("/"))
    ):
        raise ValueError(f"{name} seems to carry an API key; use a keyless public RPC URL")
    return value


@dataclass(frozen=True)
class MintableToken:
    """A test token anyone can mint: the page's faucet helper mints it from the visitor's wallet,
    and the deposit driver mints what it pays. Its own `mint(address,uint256)` is public (a
    `MockERC20`), or, with `minter`, a faucet contract's public `mint(token, to, amount)` mints it
    (Aave's testnet faucets)."""

    symbol: str
    address: str
    minter: str | None = None

    def __post_init__(self) -> None:
        if not isinstance(self.symbol, str) or not SYMBOL.fullmatch(self.symbol):
            raise ValueError("a test token's symbol must be 1-11 uppercase letters or digits")
        object.__setattr__(self, "address", checksum_address(self.address, "test token address"))
        if self.minter is not None:
            object.__setattr__(self, "minter", checksum_address(self.minter, "test token minter"))


@dataclass(frozen=True)
class ChainConfig:
    """A network the product takes payments on: its display name, a keyless public RPC (block
    times; the driver's transactions), the account's treasury there (a pin), and its mintable test
    tokens. The factory and implementation are the same on every chain (deploy/CONTRACTS.md)."""

    chain_id: int
    name: str
    rpc_url: str
    treasury: str
    test_tokens: tuple[MintableToken, ...] = ()

    def __post_init__(self) -> None:
        if type(self.chain_id) is not int or self.chain_id <= 0:
            raise ValueError("chain_id must be a positive integer")
        if not isinstance(self.name, str) or not 0 < len(self.name.strip()) <= 40:
            raise ValueError(f"chain {self.chain_id}: name must be 1-40 characters")
        keyless_rpc_url(self.rpc_url, f"chain {self.chain_id}: rpc_url")
        treasury = checksum_address(self.treasury, f"chain {self.chain_id}: treasury")
        object.__setattr__(self, "treasury", treasury)
        tokens = tuple(
            token if isinstance(token, MintableToken) else MintableToken(**token)
            for token in self.test_tokens
        )
        if len({token.symbol for token in tokens}) != len(tokens):
            raise ValueError(f"chain {self.chain_id}: test token symbols must be distinct")
        if tokens and tokens[0].minter is not None:
            raise ValueError(
                f"chain {self.chain_id}: the first test token, which the deposit driver mints"
                " and pays with, must mint itself (no minter)"
            )
        object.__setattr__(self, "test_tokens", tokens)

    @property
    def test_token(self) -> MintableToken:
        """The token the deposit driver pays with: the chain's first test token."""
        if not self.test_tokens:
            raise ValueError(f"chain {self.chain_id} has no test token to pay with")
        return self.test_tokens[0]


@dataclass(frozen=True)
class ProductConfig:
    """Everything the product needs; see deploy/sandbox/README.md for each field.

    The product's Phala Pay API key comes from `api_key_file`, or, in a CVM, from the sealed
    environment variable named by `api_key_env`: a restricted key (`ppay_rk_test_…`) holding only
    the permissions the product uses (deploy/phala.md, "Staging reference product"), or a secret key
    (`ppay_sk_test_…`). `account` is the product's Phala Pay account id (`acct_…`): the account its
    webhooks must name and the first input of every address's salt. `account`, `factory`,
    `implementation`, and each of `chains`' `treasury` are the pins every quote and deposit address
    is recomputed from. The deposit driver needs no Phala Pay key: it calls the product's account
    API at `public_url`, signed with the driver key whose public key is `driver_public_key`.

    `bonus_bps` is the product's own promotion, not a Phala Pay feature: a bonus credit, in basis
    points of the credit, for deposits of each asset (`{"pha": 1000}`: +10% on credits paid in
    PHA), clawed back with refunds and reversals (reference_product.fulfillment).
    """

    service_url: str
    account: str
    chains: tuple[ChainConfig, ...]
    factory: str
    implementation: str
    public_url: str
    listen_host: str = "127.0.0.1"
    listen_port: int = 8089
    api_key_file: str | None = None
    api_key_env: str | None = None
    ledger_path: str = ":memory:"
    driver_public_key: str | None = None
    payer: str | None = None
    payer_account: str | None = None
    unsupported_token: str | None = None
    # The account's webhook public keys (`whpk_…`) in the API key's mode, current first; unset, the
    # product pins them from `GET /v1/attestation` with its API key.
    webhook_public_keys: list[str] | None = None
    per_deposit_cap_minor: int = 100_000
    per_period_cap_minor: int = 500_000
    period_seconds: int = 24 * 60 * 60
    restart_command: list[str] = field(default_factory=list)
    # The website's origin (https://pay.phala.com), the only origin the demo's API
    # (reference_product.demo) allows; unset, the demo's API is not served.
    web_origin: str | None = None
    bonus_bps: dict[str, int] = field(default_factory=dict)

    def __post_init__(self) -> None:
        if not ACCOUNT_ID.fullmatch(self.account):
            raise ValueError("account must be the product's Phala Pay account id, acct_…")
        if self.web_origin is not None and not WEB_ORIGIN.fullmatch(self.web_origin):
            raise ValueError("web_origin must be https://HOST[:PORT] in lowercase, with no path")
        chains = tuple(
            chain if isinstance(chain, ChainConfig) else ChainConfig(**chain)
            for chain in self.chains
        )
        if not chains:
            raise ValueError("chains must name at least one chain")
        if len({chain.chain_id for chain in chains}) != len(chains):
            raise ValueError("chains must not repeat a chain_id")
        object.__setattr__(self, "chains", chains)
        object.__setattr__(self, "factory", checksum_address(self.factory, "factory"))
        implementation = checksum_address(self.implementation, "implementation")
        object.__setattr__(self, "implementation", implementation)
        for asset, bps in self.bonus_bps.items():
            if not ASSET_CODE.fullmatch(asset):
                raise ValueError("bonus_bps keys must be the service's lowercase asset codes")
            if type(bps) is not int or not 0 < bps <= 10_000:
                raise ValueError(f"bonus_bps[{asset!r}] must be an integer from 1 to 10000")

    @classmethod
    def load(cls, path: str | Path) -> ProductConfig:
        values = json.loads(Path(path).read_text(encoding="utf-8"))
        return cls(**values)

    def chain(self, chain_id: int | None = None) -> ChainConfig:
        """The configured chain `chain_id`, or the first one when `None`."""
        if chain_id is None:
            return self.chains[0]
        for chain in self.chains:
            if chain.chain_id == chain_id:
                return chain
        raise ValueError(f"chain {chain_id} is not configured")

    def treasuries(self) -> dict[int, str]:
        """The treasury pin of every configured chain, as `TopupClient` takes them."""
        return {chain.chain_id: chain.treasury for chain in self.chains}

    def api_key(self) -> str:
        if self.api_key_file is not None:
            return Path(self.api_key_file).read_text(encoding="ascii").strip()
        key = os.environ.get(self.api_key_env or "", "").strip()
        if not key:
            raise MissingProductKeyError("no api_key_file, and api_key_env is unset")
        return key

    def livemode(self) -> bool:
        """The mode of the product's API key, and so of its webhooks."""
        return self.api_key().startswith(("ppay_sk_live_", "ppay_rk_live_"))

    def client(self, *, transport: DeadlineTransport | None = None) -> TopupClient:
        # Every open quote's and active deposit address's address is recomputed from the pins
        # before it is used; a mismatch raises, as does a chain without a treasury pin.
        return TopupClient(
            self.service_url,
            self.api_key(),
            account=self.account,
            forwarder=(self.factory, self.implementation),
            treasuries=self.treasuries(),
            timeout=5,
            max_attempts=1,
            transport=transport if transport is not None else DeadlineTransport(),
        )
