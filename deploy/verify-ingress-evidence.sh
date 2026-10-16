#!/bin/sh
set -eu

# Verifies the certificate evidence that dstack-ingress publishes at https://DOMAIN/evidences/
# (deploy/README.md, "Custom domain"), the chain of the dstack-ingress README:
# 1. sha256sum.txt lists acme-account.json and cert-DOMAIN.pem, and both match it; other entries
#    may only be cert-*.pem of domains the app served before (the certificate volume keeps them);
# 2. quote.json, verified by the official dstack verifier (dstack-verifier.sh), is a valid TDX quote
#    of app APP_ID whose report_data is SHA-256(sha256sum.txt) zero-padded to 64 bytes;
# 3. the certificate DOMAIN serves on 443 is the leaf of cert-DOMAIN.pem.
# So the key TLS terminates with was obtained inside a CVM of APP_ID. The quote dates from the last
# evidence publication, so it may attest an earlier compose of the app. Prints the ACME account
# URI, which an optional CAA record pins.
if [ "$#" -ne 2 ]; then
    echo "usage: verify-ingress-evidence.sh DOMAIN APP_ID" >&2
    exit 64
fi

root=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
domain=$1
app_id=$(printf '%s' "${2#0x}" | tr 'A-F' 'a-f')
tmp=$(mktemp -d)

cleanup() {
    find "$tmp" -depth -delete
}
trap cleanup EXIT INT TERM

for file in quote.json sha256sum.txt acme-account.json "cert-$domain.pem"; do
    # Ingress withdraws evidence with 404 until it can republish it. Retry within the same
    # three-retry, 30-seconds-per-attempt bound; Deploy also enforces a ten-minute outer deadline.
    curl -fsS --max-time 30 --retry 3 --retry-all-errors -o "$tmp/$file" "https://$domain/evidences/$file"
done

awk -v cert="cert-$domain.pem" '
    $2 == "acme-account.json" || $2 == cert { print; found++; next }
    $2 !~ /^cert-[a-z0-9.-]+\.pem$/ { bad = 1 }
    END { exit (found == 2 && !bad) ? 0 : 1 }
' "$tmp/sha256sum.txt" >"$tmp/checked.txt" || {
    echo "sha256sum.txt does not list acme-account.json and cert-$domain.pem," \
        "or lists a file other than certificates" >&2
    exit 1
}
(cd "$tmp" && sha256sum --check --quiet checked.txt) || {
    echo "the evidence files do not match sha256sum.txt" >&2
    exit 1
}

jq -e '{attestation: null, quote, event_log, vm_config}
    | select((.quote | type) == "string" and (.vm_config | type) == "string")' \
    "$tmp/quote.json" >"$tmp/request.json" || {
    echo "quote.json lacks the quote or the vm_config" >&2
    exit 1
}
"$root/deploy/dstack-verifier.sh" <"$tmp/request.json" >"$tmp/result.json"
manifest=$(sha256sum "$tmp/sha256sum.txt" | cut -c1-64)
jq -e --arg app_id "$app_id" --arg report_data "$manifest" '
    .details.app_info.app_id == $app_id and .details.report_data == $report_data + ("0" * 64)
' "$tmp/result.json" >/dev/null || {
    echo "the evidence quote is not app $app_id binding sha256sum.txt" >&2
    jq '.details | {tcb_status, app_id: .app_info.app_id, report_data}' "$tmp/result.json" >&2
    exit 1
}

# The handshake is bounded: right after a DNS change the domain may not answer yet.
timeout 30 openssl s_client -connect "$domain:443" -servername "$domain" </dev/null \
    >"$tmp/served.txt" 2>/dev/null || true
openssl x509 -in "$tmp/served.txt" -outform DER >"$tmp/served.der" 2>/dev/null || {
    echo "$domain:443 served no certificate within 30 seconds" >&2
    exit 1
}
served=$(sha256sum "$tmp/served.der" | cut -c1-64)
published=$(openssl x509 -in "$tmp/cert-$domain.pem" -outform DER | sha256sum | cut -c1-64)
[ "$served" = "$published" ] || {
    echo "$domain serves a certificate other than the one in its evidence" >&2
    exit 1
}

jq -r '"dstack verifier: quote and TCB \(.details.tcb_status), app id \(.details.app_info.app_id)",
    "evidence compose hash: 0x\(.details.app_info.compose_hash)"' "$tmp/result.json"
jq -r '"ACME account: \(.registration.accountURL // .registration.uri)"' "$tmp/acme-account.json"
echo "certificate evidence of $domain passed: the served certificate was obtained in app $app_id"
