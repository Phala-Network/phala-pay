# Icons

`github.svg` is GitHub's mark, `mark-github-16.svg` from [Primer Octicons](https://github.com/primer/octicons),
MIT-licensed (see [LICENCE-octicons](LICENCE-octicons)); it links to the repository, as GitHub's
logo guidelines allow.

Chain and token marks come from the SDK (`NetworkIcon` and `AssetIcon` from `@phala/pay-react`),
inline SVG embedding CoinMarketCap's logos as `data:` images, which the page's CSP allows with
`img-src data:` (`public/_headers`).
