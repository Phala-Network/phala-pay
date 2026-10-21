import { describe, expect, it } from "vitest";
import {
  addressUrl,
  assetName,
  isTestnet,
  paymentOptionLabel,
  paymentOptions,
  samePaymentOption,
} from "../src/index.js";

describe("paymentOptions", () => {
  it("returns a flat list's own objects, in order, each option once", () => {
    const usdc = { chain_id: 1, asset: "usdc", min_amount: 500 };
    const pha = { chain_id: 1, asset: "pha", min_amount: 100 };
    const base = { chain_id: 8453, asset: "usdc", min_amount: 500 };
    const options = paymentOptions([usdc, pha, { ...usdc, asset: "USDC" }, base]);
    expect(options).toHaveLength(3);
    expect(options[0]).toBe(usdc);
    expect(options[1]).toBe(pha);
    expect(options[2]).toBe(base);
  });

  it("flattens networks network by network, keeping the network and token objects", () => {
    const sepolia = { chain_id: 11155111, assets: [{ asset: "pha" }, { asset: "usdc" }] };
    const baseSepolia = { chain_id: 84532, assets: [{ asset: "usdc" }] };
    const options = paymentOptions([sepolia, baseSepolia]);
    expect(options.map(paymentOptionLabel)).toEqual(["PHA · Sepolia", "USDC · Sepolia", "USDC · Base Sepolia"]);
    expect(options[1]?.network).toBe(sepolia);
    expect(options[1]?.token).toBe(sepolia.assets[1]);
  });
});

it("compares options by chain and case-insensitive asset code", () => {
  expect(samePaymentOption({ chain_id: 1, asset: "usdc" }, { chain_id: 1, asset: " USDC" })).toBe(true);
  expect(samePaymentOption({ chain_id: 1, asset: "usdc" }, { chain_id: 8453, asset: "usdc" })).toBe(false);
  expect(samePaymentOption({ chain_id: 1, asset: "usdc" }, null)).toBe(false);
});

it("names tokens and networks for display", () => {
  expect(paymentOptionLabel({ chain_id: 1, asset: "usdc" })).toBe("USDC · Ethereum");
  expect(paymentOptionLabel({ chain_id: 999, asset: "dai" })).toBe("DAI · Chain 999");
  expect(assetName("USDT")).toBe("Tether USD");
  expect(assetName("pha")).toBe("Phala Network");
  expect(assetName("dai")).toBe("DAI");
  expect(assetName("constructor")).toBe("CONSTRUCTOR");
  expect(isTestnet(84532)).toBe(true);
  expect(isTestnet(8453)).toBe(false);
  expect(isTestnet(999)).toBe(false);
  expect(addressUrl(8453, "0xabc")).toBe("https://basescan.org/address/0xabc");
  expect(addressUrl(999, "0xabc")).toBeUndefined();
});
