"use client";

import { useId, type ReactNode } from "react";
import { assetSymbol, isTestnet, networkName, paymentOptionLabel, samePaymentOption, type PaymentOption } from "@phala/pay";
import { rootProps, type Appearance } from "./appearance.js";
import { AssetIcon, NetworkIcon } from "./Icons.js";

export interface PaymentOptionPickerProps<T extends PaymentOption> {
  /** The tokens and networks to offer, in order; `paymentOptions()` builds them from your config. */
  options: readonly T[];
  /** The chosen option, compared by chain and asset code; no option is checked when it matches none. */
  value: PaymentOption | null | undefined;
  /** Called with the payer's choice: the very object of `options` they chose. */
  onChange: (option: T) => void;
  disabled?: boolean | undefined;
  /** The group's visible label; default "Pay with". Ignored with `aria-labelledby`. */
  label?: ReactNode | undefined;
  /** The id of your own visible label for the group, instead of `label`. */
  "aria-labelledby"?: string | undefined;
  /** Text at the end of an option, such as its price or a bonus; keep it non-interactive. */
  renderDetail?: ((option: T) => ReactNode) | undefined;
  appearance?: Appearance | undefined;
  className?: string | undefined;
}

/**
 * The token and network to pay with, as one choice: a group of native radios, one option per row,
 * each showing the token and its network with their icons and a testnet badge. Shown however few
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

/** The picker's fieldset, for a component that is itself inside a `.pp-root`. */
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
  return (
    <fieldset className="pp-options" disabled={disabled} aria-labelledby={labelledBy}>
      {labelledBy === undefined && <legend className="pp-options__legend">{label}</legend>}
      {options.map((option, index) => {
        const testnet = isTestnet(option.chain_id);
        const detail = renderDetail?.(option);
        const described = [testnet && `${id}-${index}-testnet`, detail != null && `${id}-${index}-detail`]
          .filter(Boolean)
          .join(" ");
        return (
          <label className="pp-option" key={`${option.chain_id}:${option.asset}`}>
            <input
              className="pp-option__input"
              type="radio"
              name={`${id}-option`}
              checked={samePaymentOption(option, value)}
              onChange={() => onChange(option)}
              aria-label={paymentOptionLabel(option)}
              aria-describedby={described === "" ? undefined : described}
            />
            <span className="pp-option__main">
              <AssetIcon asset={option.asset} size={24} />
              <span className="pp-option__asset">{assetSymbol(option.asset)}</span>
              <span className="pp-option__network">
                <NetworkIcon chainId={option.chain_id} size={14} />
                {networkName(option.chain_id)}
                {testnet && (
                  <span className="pp-option__badge" id={`${id}-${index}-testnet`}>
                    Testnet
                  </span>
                )}
              </span>
            </span>
            {detail != null && (
              <span className="pp-option__detail" id={`${id}-${index}-detail`}>
                {detail}
              </span>
            )}
          </label>
        );
      })}
    </fieldset>
  );
}
