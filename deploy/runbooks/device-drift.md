# Base KMS device drift

A PPID/device change, host replacement or node move can stop key release when
`allowAnyDevice=false`. Keep that policy; pre-approve one standby device before an incident.
Admin incident pauses do not require a Safe transaction.

1. Preserve the stopped CVM and storage evidence. Ensure only one instance can write the database
   and backup prefix. Do not repeatedly restart or restore an old backup into another writer.
2. Read Cloud's device id and the TDX quote for the proposed node. Verify the quote, approved OS
   and compose with the official verifier. Confirm the measured device is the intended hardware;
   a Cloud API response alone is not sufficient evidence.
3. Prepare the exact replacement/standby compose. Check its app id is the environment's committed
   `dstack-app`; keep the original app/KMS so its derived keys remain available.
4. Generate an `addDevice` Safe batch, review its app and Base chain, and have the Safe execute it:

   ```sh
   node deploy/tools/onchain-kms.mjs batch device.json "$SAFE" "$APP" addDevice "$DEVICE_ID"
   ```

5. Run `deploy/verify-onchain-app.sh "$APP" "$SAFE" "$COMPOSE_HASH" "$DEVICE_ID"`. It checks
   owner, allowed hash/device, hardware pinning and both proxy implementation pins. Then use the
   normal prepared commit/instance recovery procedure and verify attestation and ingress evidence.
6. Check backup single-writer ownership and service health before resuming. Record the old and
   new measured device ids, quote evidence, Safe transaction and acceptance results.

An unexpected app or KmsAuth upgrade is a governance incident, not ordinary drift. Investigate
before changing implementation pins. A revoked hash needs an explicit Safe approval; an
unapproved device must never be worked around with `allowAnyDevice=true`. See the
[upgrade runbook](onchain-kms-upgrade.md) and [restore runbook](../RESTORE.md).
