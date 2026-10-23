import { cleanup, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";
import { assetIcon, networkIcon } from "../../js/src/index.js";
import { AssetIcon, NetworkIcon } from "../src/index.js";
import { ICON_IMAGES } from "../../js/src/icon-images.js";

afterEach(cleanup);

describe("icon mappings", () => {
  it("maps supported symbols case-insensitively to the CoinMarketCap logos", () => {
    const image = (svg: string) => new DOMParser().parseFromString(svg, "image/svg+xml").querySelector("image")?.getAttribute("href");
    for (const asset of ["usdc", "usdt", "pha", "eth"] as const) {
      expect(image(assetIcon(asset.toUpperCase()))).toBe(ICON_IMAGES[asset]);
      expect(assetIcon(` ${asset} `)).toBe(assetIcon(asset));
    }
    expect(image(networkIcon(1))).toBe(ICON_IMAGES.ethereum);
    expect(image(networkIcon(8453))).toBe(ICON_IMAGES.base);
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

  it("contains no active content, external references, or inline styles", () => {
    for (const svg of [...["usdc", "usdt", "pha", "eth"].map(assetIcon), networkIcon(1), networkIcon(84532), assetIcon("<"), networkIcon(999)]) {
      const host = document.createElement("div");
      host.innerHTML = svg;
      for (const element of host.querySelectorAll("*")) {
        expect(["svg", "image", "circle", "text"]).toContain(element.localName);
        for (const attribute of element.attributes) {
          expect(attribute.name).not.toMatch(/^(style|on.*)$/i);
          expect(attribute.value).not.toMatch(/url\s*\(/i);
          // The only reference is an embedded PNG: no network request, no script.
          if (/href$/i.test(attribute.name)) {
            expect(`${element.localName} ${attribute.name}`).toBe("image href");
            expect(attribute.value).toMatch(/^data:image\/png;base64,[A-Za-z0-9+/]+=*$/);
          }
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
    const images = Array.from(container.querySelectorAll("image"), (image) => image.getAttribute("href"));
    expect(images).toEqual([ICON_IMAGES.ethereum, ICON_IMAGES.pha]);
  });

  it("labels standalone assets and networks, including testnets and unknowns", () => {
    render(<><NetworkIcon chainId={84532} decorative={false} size={16} />
      <NetworkIcon chainId={999} decorative={false} />
      <AssetIcon asset="USDC" decorative={false} size={20} />
      <AssetIcon asset="xyz" decorative={false} />
      <AssetIcon asset="" decorative={false} /></>);
    expect(screen.getByRole("img", { name: "Base Sepolia" }).getAttribute("width")).toBe("16");
    expect(screen.getByRole("img", { name: "USDC" }).getAttribute("height")).toBe("20");
    expect(screen.getByRole("img", { name: "Chain 999" }).textContent).toBe("C");
    expect(screen.getByRole("img", { name: "XYZ" }).textContent).toBe("X");
    expect(screen.getByRole("img", { name: "Unknown asset" }).textContent).toBe("?");
  });

  it("uses a valid default size for invalid input", () => {
    const { container } = render(<AssetIcon asset="eth" size={NaN} />);
    expect(container.querySelector("svg")?.getAttribute("width")).toBe("18");
  });
});
