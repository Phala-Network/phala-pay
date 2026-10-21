import { PhalaPay } from "@phala/pay-server";

const pay = PhalaPay.fromEnv();

// When the payer submits <PaymentElement>:
export async function createTopUp(
  team: string,
  order: string,
  chain_id: number, // the payer's choice
  asset: string,
) {
  const quote = await pay.quotes.create({
    client_reference_id: team,
    amount: 2500, // US cents
    currency: "usd",
    chain_id,
    asset,
  }, { idempotencyKey: order });
  return pay.checkoutParams(quote);
}
