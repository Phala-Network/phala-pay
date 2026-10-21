"use client";

import { useId, type ReactNode } from "react";
import { assetSymbol, isTestnet, networkName, samePaymentOption, type PaymentOption } from "@phala/pay";
import { rootProps, type Appearance } from "./appearance.js";
import { AssetIcon, NetworkIcon } from "./Icons.js";
import { Select } from "./Select.js";

export interface PaymentOptionPickerProps<T extends PaymentOption> {
  /** The tokens and networks to offer, in order; `paymentOptions()` builds them from your config. */
  options: readonly T[];
  /** The chosen option, compared by chain and asset code; nothing is chosen when it matches none. */
  value: PaymentOption | null | undefined;
  /** Called with the payer's choice: the very object of `options` they chose. */
  onChange: (option: T) => void;
  disabled?: boolean | undefined;
  /** The group's accessible name; default "Pay with". The fields' own labels are "Token" and
   * "Network". Ignored with `aria-labelledby`. */
  label?: ReactNode | undefined;
  /** The id of your own label for the group, instead of `label`. */
  "aria-labelledby"?: string | undefined;
  /** A line under the fields about the chosen option, such as its price or a bonus; keep it
   * non-interactive. */
  renderDetail?: ((option: T) => ReactNode) | undefined;
  appearance?: Appearance | undefined;
  className?: string | undefined;
}

/**
 * The token and network to pay with: a "Token" field, then a "Network" field listing the networks
 * that take the chosen token, each a select-only combobox showing icons, side by side where there is
 * room. Changing the token keeps the network where it takes the new token. Shown however few
 * options there are, so that the payer sees what they pay with. A controlled component.
 */
export function PaymentOptionPicker<T extends PaymentOption>({
  appearance,
  className,
  ...props
}: PaymentOptionPickerProps<T>) {
  return (
    <div {...rootProps(appearance, className)}>
      <PaymentOptionFields {...props} />
    </div>
  );
}

/** The picker's fields, for a component that is itself inside a `.pp-root`. */
export function PaymentOptionFields<T extends PaymentOption>({
  options,
  value,
  onChange,
  disabled = false,
  label = "Pay with",
  "aria-labelledby": labelledBy,
  renderDetail,
}: Omit<PaymentOptionPickerProps<T>, "appearance" | "className">) {
  const id = useId();
  const option = options.find((each) => samePaymentOption(each, value));
  const sameAsset = (each: PaymentOption, asset: string | undefined) =>
    asset !== undefined && each.asset.trim().toLowerCase() === asset.trim().toLowerCase();
  // Each token once, in the options' order; the networks that take the chosen one (all of them
  // until one is chosen).
  const tokens = options.filter((each, index) => options.findIndex((other) => sameAsset(other, each.asset)) === index);
  const networks = options.filter((each, index) =>
    option === undefined
      ? options.findIndex((other) => other.chain_id === each.chain_id) === index
      : sameAsset(each, option.asset),
  );
  const detail = option === undefined ? null : renderDetail?.(option);
  const detailId = `${id}-detail`;
  const choose = (found: T | undefined) => {
    if (found !== undefined && !samePaymentOption(found, option)) onChange(found);
  };
  return (
    <div className="pp-options" role="group" aria-labelledby={labelledBy ?? `${id}-label`}>
      {labelledBy === undefined && (
        <span className="pp-sr-only" id={`${id}-label`}>
          {label}
        </span>
      )}
      <div className="pp-options__fields">
        <Select
          label="Token"
          items={tokens.map((each) => ({
            value: each.asset.trim().toLowerCase(),
            label: assetSymbol(each.asset),
            icon: <AssetIcon asset={each.asset} size={20} />,
          }))}
          value={option?.asset.trim().toLowerCase()}
          onChange={(asset) =>
            choose(
              options.find((each) => sameAsset(each, asset) && each.chain_id === option?.chain_id) ??
                options.find((each) => sameAsset(each, asset)),
            )
          }
          disabled={disabled}
          describedBy={detail == null ? undefined : detailId}
        />
        <Select
          label="Network"
          items={networks.map((each) => ({
            value: String(each.chain_id),
            label: networkName(each.chain_id),
            icon: <NetworkIcon chainId={each.chain_id} size={20} />,
          }))}
          value={option === undefined ? undefined : String(option.chain_id)}
          onChange={(chain) =>
            choose(
              options.find((each) => String(each.chain_id) === chain && sameAsset(each, option?.asset)) ??
                options.find((each) => String(each.chain_id) === chain),
            )
          }
          disabled={disabled}
          suffix={option !== undefined && isTestnet(option.chain_id) ? "Testnet" : undefined}
          describedBy={detail == null ? undefined : detailId}
        />
      </div>
      {detail != null && (
        <p className="pp-options__detail" id={detailId}>
          {detail}
        </p>
      )}
    </div>
  );
}
