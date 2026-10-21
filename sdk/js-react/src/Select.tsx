"use client";

import { useEffect, useId, useLayoutEffect, useRef, useState, type KeyboardEvent, type ReactNode } from "react";
import { CheckGlyph, ChevronGlyph } from "./Glyphs.js";

export interface SelectItem {
  value: string;
  label: string;
  icon: ReactNode;
}

// Where the browser has popovers, the open listbox is in the top layer: no `overflow: hidden`
// ancestor (a card, a modal) clips it, while it stays in place in the document, inheriting the
// theme, and inside any dialog it is in. Elsewhere it is positioned under the field in the page.
const POPOVER = typeof HTMLElement !== "undefined" && "showPopover" in HTMLElement.prototype;
const GAP = 4;
const MARGIN = 8;

/**
 * A select-only combobox, as the WAI-ARIA Authoring Practices describe it: a focusable field that
 * shows the chosen item, and a listbox of items with icons that opens under it, or over it where
 * there is more room above. Focus stays on the field; the active item is its
 * `aria-activedescendant`. Arrows, Home, End, Page Up and Down move, typing finds an item, Enter,
 * Space, or Tab chooses, Escape, a press elsewhere, or leaving the field closes.
 */
export function Select({
  label,
  items,
  value,
  onChange,
  disabled = false,
  suffix,
  describedBy,
}: {
  label: string;
  items: readonly SelectItem[];
  value: string | undefined;
  onChange: (value: string) => void;
  disabled?: boolean;
  /** Muted text after the chosen item, for example "Testnet". */
  suffix?: string | undefined;
  describedBy?: string | undefined;
}) {
  const id = useId();
  const [open, setOpen] = useState(false);
  const [active, setActive] = useState(0);
  const typed = useRef({ text: "", at: 0 });
  const selectedIndex = items.findIndex((item) => item.value === value);
  const selected = items[selectedIndex];
  const last = items.length - 1;
  const optionId = (index: number) => `${id}-option-${index}`;
  const field = useRef<HTMLDivElement>(null);

  const list = useRef<HTMLUListElement>(null);
  // The open listbox in the top layer, beside the field, on whichever side has room; kept there as
  // the page scrolls or resizes. Its place is set through the CSSOM, which `style-src 'self'`
  // allows, as no style attribute in the markup would be.
  useLayoutEffect(() => {
    const box = list.current;
    const control = field.current;
    if (!POPOVER || !open || box === null || control === null) return;
    box.setAttribute("popover", "manual");
    box.showPopover();
    const place = () => {
      const rect = control.getBoundingClientRect();
      const below = window.innerHeight - rect.bottom - GAP - MARGIN;
      const above = rect.top - GAP - MARGIN;
      const up = box.scrollHeight > below && above > below;
      box.style.setProperty("left", `${rect.left}px`);
      box.style.setProperty("width", `${rect.width}px`);
      box.style.setProperty("top", up ? "auto" : `${rect.bottom + GAP}px`);
      box.style.setProperty("bottom", up ? `${window.innerHeight - rect.top + GAP}px` : "auto");
      box.style.setProperty("--pp-select-room", `${Math.max(up ? above : below, 0)}px`);
      box.dataset["side"] = up ? "top" : "bottom";
    };
    place();
    window.addEventListener("scroll", place, true);
    window.addEventListener("resize", place);
    return () => {
      window.removeEventListener("scroll", place, true);
      window.removeEventListener("resize", place);
      box.hidePopover();
      // Closed, it keeps no place of its own.
      box.removeAttribute("style");
    };
  }, [open]);

  // A press anywhere else closes it, as a tap on a blank part of a phone's page does not blur.
  useEffect(() => {
    if (!open) return;
    const outside = (event: PointerEvent) => {
      if (!(event.target instanceof Node) || field.current?.parentElement?.contains(event.target) !== true) setOpen(false);
    };
    document.addEventListener("pointerdown", outside);
    return () => document.removeEventListener("pointerdown", outside);
  }, [open]);

  // The active item stays in view as it moves.
  useEffect(() => {
    const box = list.current;
    const item = box?.children[active];
    if (!open || box === null || !(item instanceof HTMLElement)) return;
    if (item.offsetTop < box.scrollTop) box.scrollTop = item.offsetTop;
    else if (item.offsetTop + item.offsetHeight > box.scrollTop + box.clientHeight) {
      box.scrollTop = item.offsetTop + item.offsetHeight - box.clientHeight;
    }
  }, [open, active]);

  const show = (index: number) => {
    if (items.length === 0) return;
    setActive(Math.min(Math.max(index, 0), last));
    setOpen(true);
  };
  const choose = (index: number) => {
    setOpen(false);
    const item = items[index];
    if (item !== undefined && item.value !== value) onChange(item.value);
  };
  /** The next item, from the active one, whose label starts with what was typed within 500 ms. */
  const find = (key: string) => {
    const now = Date.now();
    typed.current = { text: (now - typed.current.at < 500 ? typed.current.text : "") + key.toLowerCase(), at: now };
    const from = open ? active : Math.max(selectedIndex, 0);
    const order = [...items.keys()].map((offset) => (from + (typed.current.text.length === 1 ? offset + 1 : offset)) % items.length);
    return order.find((index) => items[index]?.label.toLowerCase().startsWith(typed.current.text));
  };

  const onKeyDown = (event: KeyboardEvent<HTMLDivElement>) => {
    if (disabled || items.length === 0) return;
    const current = open ? active : Math.max(selectedIndex, 0);
    const move = (index: number) => {
      event.preventDefault();
      show(index);
    };
    const key = event.key;
    if (key === "ArrowDown") {
      move(open && !event.altKey ? current + 1 : current);
    } else if (key === "ArrowUp" && open && event.altKey) {
      event.preventDefault();
      choose(active);
    } else if (key === "ArrowUp") {
      move(open ? current - 1 : 0);
    } else if (key === "Home") {
      move(0);
    } else if (key === "End") {
      move(last);
    } else if ((key === "PageDown" || key === "PageUp") && open) {
      move(current + (key === "PageDown" ? 10 : -10));
    } else if (key === "Enter" || key === " ") {
      event.preventDefault();
      if (open) choose(active);
      else show(current);
    } else if (key === "Escape" && open) {
      event.preventDefault();
      setOpen(false);
    } else if (key === "Tab" && open) {
      choose(active);
    } else if (key.length === 1 && !event.ctrlKey && !event.metaKey && !event.altKey) {
      move(find(key) ?? current);
    }
  };

  return (
    <div className="pp-select">
      {/* A press on the label focuses its field, as a label does. */}
      <span className="pp-select__label" id={`${id}-label`} onClick={() => field.current?.focus()}>
        {label}
      </span>
      <div className="pp-select__control">
        <div
          ref={field}
          role="combobox"
          id={`${id}-combobox`}
          className="pp-select__trigger"
          tabIndex={disabled ? -1 : 0}
          aria-labelledby={`${id}-label`}
          aria-controls={`${id}-listbox`}
          aria-expanded={open}
          aria-haspopup="listbox"
          aria-activedescendant={open && items.length > 0 ? optionId(active) : undefined}
          aria-disabled={disabled || undefined}
          aria-describedby={describedBy}
          onClick={() => {
            if (disabled) return;
            if (open) setOpen(false);
            else show(Math.max(selectedIndex, 0));
          }}
          onKeyDown={onKeyDown}
          onBlur={() => setOpen(false)}
        >
          {selected === undefined ? (
            <span className="pp-select__placeholder">Choose</span>
          ) : (
            <>
              {selected.icon}
              <span className="pp-select__value" title={selected.label}>{selected.label}</span>
              {suffix !== undefined && <> <span className="pp-select__suffix">{suffix}</span></>}
            </>
          )}
          <ChevronGlyph />
        </div>
        <ul ref={list} role="listbox" id={`${id}-listbox`} className="pp-select__listbox" aria-labelledby={`${id}-label`} tabIndex={-1} hidden={!open}>
          {items.map((item, index) => (
            <li
              key={item.value}
              role="option"
              id={optionId(index)}
              className="pp-select__option"
              aria-selected={index === selectedIndex}
              data-active={open && index === active ? "" : undefined}
              // The field keeps the focus: a press chooses without blurring it.
              onMouseDown={(event) => event.preventDefault()}
              onClick={() => choose(index)}
            >
              {item.icon}
              <span className="pp-select__value" title={item.label}>{item.label}</span>
              {index === selectedIndex && (
                <span className="pp-select__check">
                  <CheckGlyph />
                </span>
              )}
            </li>
          ))}
        </ul>
      </div>
    </div>
  );
}
