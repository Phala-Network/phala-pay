# Phala Pay brand

The logo is the green dot: a lime square in a near-black tile.

| File | Use |
|---|---|
| [`mark-light.svg`](mark-light.svg) | The mark, on light backgrounds |
| [`mark-dark.svg`](mark-dark.svg) | The mark, on dark backgrounds: the tile gets an edge |
| [`lockup-light.svg`](lockup-light.svg) | The mark and the name, on light backgrounds |
| [`lockup-dark.svg`](lockup-dark.svg) | The mark and the name, on dark backgrounds |
| [`app-icon.svg`](app-icon.svg) | The mark full-bleed, for the Apple touch icon and Android's maskable icon |
| [`og-image.svg`](og-image.svg) | The link preview |

## Construction

- **Grid.** The mark is 32 × 32 units. The tile's corner radius is 8. The dot is 12 × 12 at
  (10, 10), so centred on (16, 16), with a radius of 3. The dot is the tile at 3/8 scale, so both
  have the same corner ratio, and every edge falls on whole pixels at 16 and 32 px.
- **Colour.** The tile is `#0a0a0a`, the site's dark background. The dot is `#cefc5d`, the site's
  `--brand` token `oklch(0.93 0.19 124)`. Use `mark-dark` on any background darker than about
  zinc-600 (`#52525b`).
- **Edge.** On dark backgrounds the tile gets an edge: a ring inside its outline, in white at 15%
  opacity. It is a filled even-odd path, not a stroke, so design tools and browsers draw it alike.
  Each file's ring is about 1 px at the size the file is used at:
  - `mark-dark.svg`, `lockup-dark.svg` and the site's lockup: 1 unit, 1 px at their natural 32 px
    mark (2 px in the link preview's 64 px mark);
  - `public/favicon.svg`, which shows the edge in a dark browser theme: 2 units, 1 px in a 16 px
    tab, on whole pixels at 1x and 2x.
- **Lockup.** The lockup is [Phala's logo](https://phala.com/home/logo.svg) with our mark in
  place of Phala's: it keeps that logo's units, a 48-unit mark and 16-unit caps centred on it, 10.67
  units away. The files draw the mark at 1.5 times its 32-unit grid, and display at 128 × 32, so
  the mark is its natural 32 px. The name is `#0a0a0a` on light backgrounds and `#fafafa` on dark
  ones.
- **Clear space and size.** Keep 8 units of the mark's grid clear around the mark or lockup. Use the
  mark at 16 px or larger, and the lockup with a mark of 24 px or larger.

The site's header and footer draw the mark inline at 24 px (`Lockup` in `src/Site.tsx`) beside the name
set in the site's typeface (Geist, semibold), as a product name sits beside its mark in an
interface; the lettered lockup stays for the link preview and these files.

## Lettering

The name, PHALA PAY, is set in the lettering of [Phala's logo](https://phala.com/home/logo.svg):
Phala's own brand, used by a Phala product. The letters are outlines, not a font, so nothing is
embedded or licensed separately.

- **P, H, A, L.** Phala's outlines, copied verbatim from `logo.svg`. PHALA keeps their
  positions, so it is Phala's wordmark exactly; the P and A of PAY are the same outlines, moved
  along the baseline (`translate`).
- **Y.** Phala's logo has no Y, so it is built from the A. Its arms are the A's legs turned 180°,
  the right leg becoming the left arm, so they have the A's angles (23.1° and 23.5° from vertical
  outside, 22.0° and 22.2° inside), weights (3.87 and 3.73 units across the cut) and taper, and end
  in the A's feet: flat cuts, now on the cap line. The stem is 3.61 units, the mean of the stems of
  P, H and L, and ends flat on the baseline. The arms meet the stem, on average, at the underside
  of the H's bar, 4.8 units up, which puts the crotch at 9.01 units, just above the middle.
- **Spacing.** PHALA keeps Phala's spacing and kerning. Measured as the closest distance between
  outlines, its pairs are PH 2.05, HA 1.51, AL 1.65 and LA 0.80 units. PAY is spaced by eye to
  the same colour, then checked blurred against PHALA. PA and AY cannot reach those distances,
  since the P's bowl and the A's and Y's diagonals leave open space on both sides of the pair, so
  they are kerned the way a text face kerns them. The A sits 0.21 units from the P's bowl at their
  bounding boxes, 3.75 at the closest outlines. The Y's arm tip overhangs the A's foot by 0.55
  units, 4.33 at the closest outlines. Spacing those pairs by white area, with each letter's
  recesses counted only 1.1 units deep, undercounted their open sides and left them at 4.39 and
  5.53 units, visibly looser than PHALA.
- **Word space.** 7.95 units from the last A's foot to the P's stem, about half the cap height.

## Rendering

`pnpm run brand` renders `public/favicon-32.png`, the manifest's icons, the Apple touch icon, and
`public/og-image.png` from these SVGs and `public/favicon.svg`; commit the PNGs. The icons that
platforms mask are full-bleed, and the dot stays well inside Android's maskable safe zone, the
centred circle of 80% diameter.
