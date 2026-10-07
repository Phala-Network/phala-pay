import type { CheckoutParams } from "@phala/pay";
import { Checkout } from "@phala/pay-react";
import "@phala/pay-react/styles.css";

export function TopUp({ checkout, onPaid }: {
  checkout: CheckoutParams;
  onPaid: () => void;
}) {
  return (
    <Checkout {...checkout} onSuccess={onPaid} />
  );
}
