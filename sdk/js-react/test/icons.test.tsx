import { cleanup, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";
import { assetIcon, networkIcon } from "../../js/src/index.js";
import { AssetIcon, NetworkIcon } from "../src/index.js";
import { ICON_SVG } from "../../js/src/icon-svg.js";

afterEach(cleanup);

describe("icon mappings", () => {
  it("maps supported symbols case-insensitively to the branded SVGs", () => {
    for (const asset of ["usdc", "usdt", "pha", "eth"] as const) {
      expect(assetIcon(asset.toUpperCase())).toBe(ICON_SVG[asset]);
      expect(assetIcon(` ${asset} `)).toBe(ICON_SVG[asset]);
    }
    expect(networkIcon(1)).toBe(ICON_SVG.ethereum);
    expect(networkIcon(8453)).toBe(ICON_SVG.base);
  });

  it("uses mainnet artwork for every registered testnet", () => {
    expect(networkIcon(11155111)).toBe(networkIcon(1));
    expect(networkIcon(84532)).toBe(networkIcon(8453));
  });

  it("falls back to neutral monograms and escapes untrusted asset symbols", () => {
    expect(assetIcon("xyz")).toContain('>X</text>');
    expect(assetIcon("")).toContain('>?</text>');
    expect(assetIcon("<script>")).toContain('>&lt;</text>');
    expect(networkIcon(999)).toContain('>C</text>');
  });

  it("contains no active content, references, or inline styles", () => {
    for (const svg of [...Object.values(ICON_SVG), assetIcon("<"), networkIcon(84532)]) {
      const host = document.createElement("div");
      host.innerHTML = svg;
      for (const element of host.querySelectorAll("*")) {
        expect(["svg", "path", "circle", "text", "span"]).toContain(element.localName);
        for (const attribute of element.attributes) {
          expect(attribute.name).not.toMatch(/^(style|on.*|.*href)$/i);
          expect(attribute.value).not.toMatch(/url\s*\(/i);
        }
      }
    }
  });
});

describe("React icons", () => {
  it("keeps artwork decorative beside text", () => {
    const { container } = render(<><NetworkIcon chainId={11155111} /> Sepolia <AssetIcon asset="pha" /> PHA</>);
    expect(screen.queryByRole("img")).toBeNull();
    expect(container.textContent).toContain("Sepolia");
    for (const svg of container.querySelectorAll("svg")) {
      expect(svg.getAttribute("aria-hidden")).toBe("true");
      expect(svg.getAttribute("width")).toBe("18");
      expect(svg.hasAttribute("style")).toBe(false);
    }
  });

  it("labels standalone assets and networks, including testnets and unknowns", () => {
    render(<><NetworkIcon chainId={84532} decorative={false} size={16} />
      <NetworkIcon chainId={999} decorative={false} />
      <AssetIcon asset="USDC" decorative={false} size={20} />
      <AssetIcon asset="xyz" decorative={false} />
      <AssetIcon asset="" decorative={false} /></>);
    expect(screen.getByRole("img", { name: "Base Sepolia" }).querySelector("svg")?.getAttribute("width")).toBe("16");
    expect(screen.getByRole("img", { name: "USDC" }).querySelector("svg")?.getAttribute("height")).toBe("20");
    expect(screen.getByRole("img", { name: "Chain 999" }).textContent).toBe("C");
    expect(screen.getByRole("img", { name: "XYZ" }).textContent).toBe("X");
    expect(screen.getByRole("img", { name: "Unknown asset" }).textContent).toBe("?");
  });

  it("uses a valid default size for invalid input", () => {
    const { container } = render(<AssetIcon asset="eth" size={NaN} />);
    expect(container.querySelector("svg")?.getAttribute("width")).toBe("18");
  });
});
