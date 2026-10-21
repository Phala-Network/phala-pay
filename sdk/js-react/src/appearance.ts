/** Base theme; customize the `--pp-*` properties on `.pp-root[data-theme]` in your stylesheet. */
export interface Appearance {
  theme?: "light" | "dark";
}

/** A component's `.pp-root` wrapper attributes: its class names and theme. */
export function rootProps(appearance: Appearance | undefined, className: string | undefined) {
  return {
    className: className === undefined ? "pp-root" : `pp-root ${className}`,
    "data-theme": appearance?.theme ?? "light",
  };
}
