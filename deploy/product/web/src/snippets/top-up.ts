import { PhalaPay } from "@phala/pay-server";

const pay = PhalaPay.fromEnv();

export async function createTopUp(team: string, order: string) {
  const quote = await pay.quotes.create({
    client_reference_id: team,
    amount: 2500, // US cents
    currency: "usd",
    chain_id: 84532, // Base Sepolia
    asset: "usdc",
  }, { idempotencyKey: order });
  return pay.checkoutParams(quote);
}
