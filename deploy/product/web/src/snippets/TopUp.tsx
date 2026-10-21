import type {
  CheckoutParams as Params,
  PaymentOption as Option,
} from "@phala/pay";
import {
  PaymentElement,
} from "@phala/pay-react";
import "@phala/pay-react/styles.css";

export function TopUp(props: {
  options: Option[]; // GET /v1/config
  quote: (option: Option) => Promise<Params>;
  onPaid: () => void;
}) {
  return (
    <PaymentElement
      options={props.options}
      amount={2500}
      createQuote={props.quote}
      onSuccess={props.onPaid}
    />
  );
}
