import { PhalaPay } from "@phala/pay-server";

const pay = PhalaPay.fromEnv();

// When the payer submits <PaymentElement>,
// with its choice and idempotency key:
export async function createTopUp(
  team: string,
  chain_id: number,
  asset: string,
  idempotencyKey: string, // a UUID: check it
) {
  const quote = await pay.quotes.create({
    client_reference_id: team,
    amount: 2500, // US cents
    currency: "usd",
    chain_id,
    asset,
  }, { idempotencyKey });
  return pay.checkoutParams(quote);
}
